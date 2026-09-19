//! End-to-end: the indexer *computes* per-height commitment-tree sizes and the
//! store serves them in the composed compact block.
//!
//! Unlike `index_boot` (empty mock blocks → all-zero sizes), this feeds blocks
//! that actually commit sapling outputs and orchard actions, and asserts the
//! served `ChainMetadata` is the running cumulative count — the `(SelfCumulative,
//! Append)` chain-metadata index doing its job through the real sync engine and
//! the store's compose-on-read. The source blocks carry `ChainMetadata::ZERO`
//! (a validator does not report per-block sizes), so a non-zero served size can
//! only come from the indexer computing it.

use std::sync::Arc;

use zaino_component::{ComponentName, Lifecycle, ReachabilityProbe};
use zaino_core::{BlockRef, Height};
use zaino_indexer::SourceSyncDriver;
use zaino_indexes::sets::current_zaino::{context_from_block, index_set};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{
    Block, CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier, OrchardAction, OrchardData,
    SaplingData, SaplingOutput, Transaction, TransactionId,
};
use zaino_runtime::{IndexerComponent, OrchestraBuilder, ValidatorComponent};
use zaino_service::{CompactBlockRead, TakeSnapshot};
use zaino_source::mock::{test_block, MockChain};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::{StoreComponent, StoreReader};

struct Probe(bool);
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

/// One transaction committing `saplings` sapling outputs, `orchards` orchard
/// actions, and `ironwoods` ironwood actions. Contents are filler — only the
/// *counts* drive tree sizes. Ironwood shares orchard's action shape.
fn shielded_tx(seed: u8, saplings: usize, orchards: usize, ironwoods: usize) -> Transaction {
    let sapling_output = || SaplingOutput {
        cmu: NoteCommitment::from([seed; 32]),
        ephemeral_key: EphemeralKey::from([seed; 32]),
        enc_ciphertext: CompactCiphertext::from([seed; CompactCiphertext::LENGTH]),
    };
    let orchard_action = || OrchardAction {
        nullifier: Nullifier::from([seed; 32]),
        cmx: NoteCommitment::from([seed; 32]),
        ephemeral_key: EphemeralKey::from([seed; 32]),
        enc_ciphertext: CompactCiphertext::from([seed; CompactCiphertext::LENGTH]),
    };
    Transaction {
        txid: TransactionId::from([seed; 32]),
        transparent: Default::default(),
        sapling: SaplingData {
            outputs: (0..saplings).map(|_| sapling_output()).collect(),
            ..Default::default()
        },
        orchard: OrchardData {
            actions: (0..orchards).map(|_| orchard_action()).collect(),
            ..Default::default()
        },
        ironwood: OrchardData {
            actions: (0..ironwoods).map(|_| orchard_action()).collect(),
            ..Default::default()
        },
    }
}

/// A mock block at `height` carrying one shielded transaction. Reuses
/// `test_block`'s header; the source reports no tree sizes (`ChainMetadata::ZERO`).
fn shielded_block(
    height: u32,
    hash_byte: u8,
    saplings: usize,
    orchards: usize,
    ironwoods: usize,
) -> Block {
    let mut block = test_block(height, hash_byte);
    block.transactions = vec![shielded_tx(hash_byte, saplings, orchards, ironwoods)];
    block
}

#[tokio::test]
async fn indexer_computes_and_serves_cumulative_tree_sizes() {
    let backend = InMemoryBackend::new();

    // Per-block commitments:  h0: 2 sapling, 1 orchard, 0 ironwood
    //                         h1: 3 sapling, 0 orchard, 2 ironwood
    //                         h2: 0 sapling, 2 orchard, 1 ironwood
    let chain = MockChain::new()
        .with_block(shielded_block(0, 1, 2, 1, 0))
        .with_block(shielded_block(1, 2, 3, 0, 2))
        .with_block(shielded_block(2, 3, 0, 2, 1));
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    let driver = SourceSyncDriver::resuming(
        &backend,
        index_set(),
        source,
        |block| context_from_block(&block),
        8,
        0,
        16,
    )
    .expect("driver builds");
    let indexer = IndexerComponent::new(ComponentName("indexer"), driver);
    let store = StoreComponent::new(
        ComponentName("store"),
        StoreReader::new(Arc::new(backend.clone())),
    );
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("validator reachable");

    let orchestra = OrchestraBuilder::new()
        .boot_observed(validator)
        .await
        .boot(indexer)
        .await
        .expect("indexer boots")
        .boot(store.clone())
        .await
        .expect("store boots")
        .build();
    for status in orchestra.statuses() {
        assert_eq!(status.lifecycle, Lifecycle::Ready, "{}", status.name);
    }

    let snapshot = store.reader().snapshot().await.expect("snapshot");

    // Served `ChainMetadata` is the *cumulative* count, computed by the indexer
    // (the source reported ZERO). Expected (height, sapling, orchard, ironwood):
    //   h0: 2, 1, 0     h1: 2+3=5, 1, 0+2=2     h2: 5, 1+2=3, 2+1=3
    for (height, sapling, orchard, ironwood) in
        [(0u32, 2u32, 1u32, 0u32), (1, 5, 1, 2), (2, 5, 3, 3)]
    {
        let block = snapshot
            .compact_block(BlockRef::Height(Height::try_from(height).expect("height")))
            .await
            .expect("compact_block read")
            .expect("a block at this height");
        assert_eq!(
            u32::from(block.chain_metadata.sapling_tree_size),
            sapling,
            "cumulative sapling size at height {height}"
        );
        assert_eq!(
            u32::from(block.chain_metadata.orchard_tree_size),
            orchard,
            "cumulative orchard size at height {height}"
        );
        assert_eq!(
            u32::from(block.chain_metadata.ironwood_tree_size),
            ironwood,
            "cumulative ironwood size at height {height}"
        );
    }
}
