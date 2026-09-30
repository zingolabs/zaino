//! Spend status composed across the seam, under a `Local` placement.
//!
//! The case that matters is the one neither tier can answer alone: an output
//! **created below the watermark** and **spent above it**. The finalised store
//! holds the output and no spend of it; the volatile window holds the spend and
//! never saw the output. Only the composition is right.
//!
//! ```text
//! FS = [0, 2] creates (A, 0) and (A, 1) at height 1
//! NFS = [3, 4] spends (A, 0) at height 3, by B
//!
//! (A, 0) → Spent { by: B }   the window's fact wins
//! (A, 1) → Unspent           neither tier saw a spend, the store has the output
//! (Z, 0) → NoSuchOutput      no tier created it
//! ```
//!
//! That this file compiles at all is half the point: a `Local` spend placement
//! requires both tiers to implement the read, which is the bound that could not
//! be satisfied before.

use std::sync::Arc;

use zaino_component::{ComponentName, Lifecycle, ReachabilityProbe};
use zaino_core::Engine;
use zaino_core::routing::{Local, Passthrough, Routing, Withheld};
use zaino_core::testing::{StubNonFinalised, stub_compact_block};
use zaino_indexer::{FetchConcurrency, SourceSyncDriver, SyncTuning};
use zaino_indexes::index_set::IndexSet;
use zaino_indexes::sets::current_zaino::{CurrentZaino, context_from_block};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{
    Block, CompactBlock, Outpoint, PreIndexCompactTx, Script, Transaction, TransactionId,
    TransparentData, TransparentInput, TransparentOutput, Zatoshis,
};
use zaino_runtime::{OrchestraBuilder, RunComponent, ValidatorComponent};
use zaino_service::{SpendRead, SpendStatus, TakeSnapshot};
use zaino_source::mock::{MockChain, test_block};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::{StoreComponent, StoreReader};

/// The routing under test: spend status composed from the tiers. Every other
/// placement is irrelevant here and is set to what a deployment would most
/// plausibly choose, so the table stays readable.
struct SpendLocally;

impl Routing for SpendLocally {
    type Address = Passthrough;
    type Treestate = Passthrough;
    type Spend = Local;
    type TransactionLocation = Withheld;
}

/// The transaction that creates the outputs, mined below the watermark.
fn creator() -> TransactionId {
    TransactionId::from([0xA1; 32])
}
/// The transaction that spends one of them, mined in the volatile window.
fn spender() -> TransactionId {
    TransactionId::from([0xB2; 32])
}
/// A transaction no tier ever saw.
fn stranger() -> TransactionId {
    TransactionId::from([0xCC; 32])
}

struct Probe;
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        true
    }
}

fn outpoint(txid: TransactionId, index: u32) -> Outpoint {
    Outpoint { txid, index }
}

/// A P2PKH script locking to `hash`.
fn script(hash: u8) -> Script {
    let mut bytes = vec![0x76, 0xa9, 0x14];
    bytes.extend_from_slice(&[hash; 20]);
    bytes.extend_from_slice(&[0x88, 0xac]);
    Script::new(bytes)
}

fn value(zats: u64) -> Zatoshis {
    Zatoshis::new(zats).expect("a valid amount")
}

/// A transaction creating two transparent outputs and spending `inputs`.
fn transaction(txid: TransactionId, inputs: Vec<TransparentInput>, outputs: usize) -> Transaction {
    Transaction {
        txid,
        transparent: TransparentData {
            inputs,
            outputs: (0..outputs)
                .map(|i| TransparentOutput {
                    value: value(1_000 + u64::try_from(i).expect("a small index")),
                    script: script(u8::try_from(i).expect("a small index")),
                })
                .collect(),
        },
        sapling: Default::default(),
        orchard: Default::default(),
        ironwood: Default::default(),
    }
}

/// The finalised store over `[0, 2]`, where height 1 mines [`creator`] with two
/// outputs. Finalised depth is zero, so the watermark is exactly 2.
async fn indexed_store() -> StoreReader<InMemoryBackend, CurrentZaino> {
    let backend = InMemoryBackend::new();
    let mut chain = MockChain::new();
    for height in 0..=2u32 {
        let hash_byte = u8::try_from(10 + height).expect("a small height");
        let mut block: Block = test_block(height, hash_byte);
        if height == 1 {
            block.transactions = vec![transaction(creator(), Vec::new(), 2)];
        }
        chain = chain.with_block(block);
    }
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    let driver = SourceSyncDriver::resuming(
        &backend,
        CurrentZaino::pipelines(),
        source,
        |block| context_from_block(&block),
        SyncTuning {
            batch_size: 8,
            finalised_depth: 0,
            channel_capacity: 16,
            concurrency: FetchConcurrency::SERIAL,
        },
    )
    .expect("the driver builds");

    let reader = StoreReader::new(Arc::new(backend.clone()));
    let orchestra = OrchestraBuilder::new()
        .boot_observed(
            ValidatorComponent::connect(&Probe)
                .await
                .expect("the validator is reachable"),
        )
        .await
        .boot(RunComponent::new(ComponentName("indexer"), driver))
        .await
        .expect("the indexer boots")
        .boot(StoreComponent::new(ComponentName("store"), reader.clone()))
        .await
        .expect("the store boots")
        .build();
    for status in orchestra.statuses() {
        assert_eq!(status.lifecycle, Lifecycle::Ready, "{}", status.name);
    }
    reader
}

/// The volatile window over `[3, 4]`, where height 3 mines [`spender`] spending
/// the first of the store's outputs.
fn window() -> StubNonFinalised {
    let spend = PreIndexCompactTx {
        txid: spender(),
        transparent_inputs: vec![TransparentInput {
            prev_txid: creator(),
            prev_index: 0,
        }],
        transparent_outputs: vec![TransparentOutput {
            value: value(900),
            script: script(9),
        }],
        sapling_nullifiers: Vec::new(),
        sapling_outputs: Vec::new(),
        orchard_actions: Vec::new(),
        ironwood_actions: Vec::new(),
    };
    let mut blocks: Vec<CompactBlock> = (3..=4u32)
        .map(|height| {
            stub_compact_block(height, u8::try_from(10 + height).expect("a small height"))
        })
        .collect();
    blocks[0].transactions = vec![spend];
    StubNonFinalised::from_blocks(blocks)
}

/// The composed engine over both tiers, under [`SpendLocally`].
async fn engine() -> impl SpendRead {
    let engine: Engine<_, _, (), SpendLocally> = Engine::new(indexed_store().await, window(), ());
    engine.snapshot().await.expect("the pin is taken")
}

/// The cross-seam case: the store created the output, the window spent it, and
/// only the composition can say so.
#[tokio::test]
async fn an_output_created_below_the_watermark_and_spent_above_it_reads_spent() {
    let status = engine()
        .await
        .spend_status(outpoint(creator(), 0))
        .await
        .expect("the read succeeds");
    assert_eq!(status, SpendStatus::Spent { by: spender() });
}

/// Its sibling output was never spent by either tier. The store holds it, so
/// the answer is `Unspent` and not `NoSuchOutput` — the distinction the store's
/// three indexes exist to make.
#[tokio::test]
async fn an_unspent_output_below_the_watermark_reads_unspent() {
    let status = engine()
        .await
        .spend_status(outpoint(creator(), 1))
        .await
        .expect("the read succeeds");
    assert_eq!(status, SpendStatus::Unspent);
}

/// No tier created it, so neither can call it unspent.
#[tokio::test]
async fn an_outpoint_no_tier_created_reads_no_such_output() {
    let status = engine()
        .await
        .spend_status(outpoint(stranger(), 0))
        .await
        .expect("the read succeeds");
    assert_eq!(status, SpendStatus::NoSuchOutput);
}

/// An output index past the end of a real transaction is not an output.
#[tokio::test]
async fn an_index_past_the_transactions_outputs_reads_no_such_output() {
    let status = engine()
        .await
        .spend_status(outpoint(creator(), 7))
        .await
        .expect("the read succeeds");
    assert_eq!(status, SpendStatus::NoSuchOutput);
}
