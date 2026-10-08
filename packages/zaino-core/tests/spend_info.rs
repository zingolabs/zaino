//! `getspentinfo`'s locating read (`SpendRead::spend_info`) composed across the
//! seam, under a `Local` placement.
//!
//! Where [`seam_spend`](super) proves the three-way spend *status*, this proves
//! the *location* the explorer's `getspentinfo` needs: the spending transaction,
//! the input of it that consumed the outpoint, and the height it was mined at.
//!
//! The two cases that matter are a spend wholly within the finalised tier and
//! one across the seam — an output created below the watermark and spent above
//! it, which only the composition can locate (the store saw no spend, the window
//! never saw the output created).
//!
//! ```text
//! FS = [0, 3]  h1 creates (creator, 0..3); h2's `fin_spender` spends (creator, 1)
//! NFS = [4, 5] h4's `spender` spends (creator, 0)
//!
//! (creator, 1) -> Some { by: fin_spender, input_index: 0, height: 2 }  same tier
//! (creator, 0) -> Some { by: spender,     input_index: 0, height: 4 }  across the seam
//! (creator, 2) -> None                                                 unspent
//! (stranger, 0) -> None                                                never created
//! ```

use std::sync::Arc;

use zaino_component::{ComponentName, Lifecycle, ReachabilityProbe};
use zaino_core::Engine;
use zaino_core::routing::{Local, Passthrough, Routing, Withheld};
use zaino_core::testing::{StubNonFinalised, stub_compact_block};
use zaino_indexer::{FetchConcurrency, SourceSyncDriver, SyncTarget, SyncTuning};
use zaino_indexes::index_set::IndexSet;
use zaino_indexes::sets::current_zaino::context_from_block;
use zaino_indexes::sets::transparent_history::TransparentHistory;
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{
    Block, CompactBlock, Height, Outpoint, OutputIndex, PreIndexCompactTx, Script, Transaction,
    TransactionId, TransparentData, TransparentInput, TransparentOutput, TransparentSpend,
    Zatoshis,
};
use zaino_runtime::{OrchestraBuilder, RunComponent, ValidatorComponent};
use zaino_service::{SpendRead, TakeSnapshot};
use zaino_source::mock::{MockChain, test_block};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::StoreReader;
use zaino_store_service::StoreComponent;

/// The routing under test: spend status composed from the tiers. Every other
/// placement is set to what a deployment would most plausibly choose.
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
/// The finalised transaction that spends one output within the store.
fn fin_spender() -> TransactionId {
    TransactionId::from([0xF3; 32])
}
/// The transaction that spends another output in the volatile window.
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

/// A transaction creating `outputs` transparent outputs and spending `inputs`.
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

/// The finalised store over `[0, 3]`: height 1 mines [`creator`] with three
/// outputs, height 2 mines [`fin_spender`] spending `(creator, 1)`. Finalised
/// depth is zero, so the watermark is exactly 3 and both are finalised.
async fn indexed_store() -> StoreReader<InMemoryBackend, TransparentHistory> {
    let backend = InMemoryBackend::new();
    let mut chain = MockChain::new();
    for height in 0..=3u32 {
        let hash_byte = u8::try_from(10 + height).expect("a small height");
        let mut block: Block = test_block(height, hash_byte);
        if height == 1 {
            block.transactions = vec![transaction(creator(), Vec::new(), 3)];
        }
        if height == 2 {
            block.transactions = vec![transaction(
                fin_spender(),
                vec![TransparentInput {
                    prev_txid: creator(),
                    prev_index: 1,
                }],
                1,
            )];
        }
        chain = chain.with_block(block);
    }
    let source = Arc::new(ValidatorClient::new(chain, RetryPolicy::default()));

    let driver = SourceSyncDriver::resuming(
        &backend,
        TransparentHistory::pipelines(),
        source,
        |block| context_from_block(&block),
        SyncTuning {
            batch_size: 8,
            channel_capacity: 16,
            concurrency: FetchConcurrency::SERIAL,
        },
        SyncTarget::Depth { depth: 0 },
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

/// The volatile window over `[4, 5]`, where height 4 mines [`spender`] spending
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
    let mut blocks: Vec<CompactBlock> = (4..=5u32)
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

/// A spend wholly within the finalised tier is located there: the spending
/// transaction, the input of it that consumed the outpoint (recovered by
/// scanning its inputs, since the index records only the spender), and the
/// height it was mined at.
#[tokio::test]
async fn a_spend_within_the_finalised_tier_is_located() {
    let spend = engine()
        .await
        .spend_info(outpoint(creator(), 1))
        .await
        .expect("the read succeeds")
        .expect("the output was spent");
    assert_eq!(
        spend,
        TransparentSpend {
            outpoint: outpoint(creator(), 1),
            by: fin_spender(),
            input_index: OutputIndex::try_from(0usize).expect("a small index"),
            height: Height::try_from(2).expect("a valid height"),
            // `fin_spender` is the only transaction in height 2's block.
            block_index: 0,
        }
    );
}

/// The seam case (Review Focus 3): an output created below the watermark and
/// spent in the volatile window is reported with the spending height, which only
/// the composition can give — the store holds the output and no spend of it, the
/// window holds the spend and never saw the output created.
#[tokio::test]
async fn a_spend_across_the_seam_reports_the_spending_height() {
    let spend = engine()
        .await
        .spend_info(outpoint(creator(), 0))
        .await
        .expect("the read succeeds")
        .expect("the output was spent in the window");
    assert_eq!(
        spend,
        TransparentSpend {
            outpoint: outpoint(creator(), 0),
            by: spender(),
            input_index: OutputIndex::try_from(0usize).expect("a small index"),
            height: Height::try_from(4).expect("a valid height"),
            // `spender` is the only transaction in the window's height-4 block.
            block_index: 0,
        }
    );
}

/// An output neither tier spent reads as `None` — the answer `getspentinfo`
/// renders as zcashd's not-found error, not as a located spend.
#[tokio::test]
async fn an_unspent_output_reads_none() {
    let spend = engine()
        .await
        .spend_info(outpoint(creator(), 2))
        .await
        .expect("the read succeeds");
    assert_eq!(spend, None);
}

/// An outpoint no tier ever created also reads `None`: `getspentinfo` collapses
/// unspent and unknown into the same not-found answer, as zcashd does.
#[tokio::test]
async fn an_unknown_outpoint_reads_none() {
    let spend = engine()
        .await
        .spend_info(outpoint(stranger(), 0))
        .await
        .expect("the read succeeds");
    assert_eq!(spend, None);
}
