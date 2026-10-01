//! Transparent address history composed across the seam, under a `Local`
//! placement.
//!
//! The case neither tier can answer alone is an output **received below the
//! watermark** and **spent in the window**. The store holds the output and no
//! spend of it; the window holds the spend and cannot say whose output it was,
//! because a transparent input names only the outpoint it consumes. Only the
//! composition is right, and only because the composer supplies the window the
//! candidate outpoints — the history the window lacks.
//!
//! ```text
//! FS  = [0, 2]  height 1 pays A twice (outputs 0 and 1) and pays B once
//! NFS = [3, 4]  height 3 spends A's output 0, and pays A again
//!
//! balance(A, [0, 4])  = output 1 (unspent) + the window's receive
//! unspent(A)          = the same two, output 0 filtered out by the window
//! tx_ids(A, [0, 4])   = the payer, the spender, the window's payer
//! ```
//!
//! That this file compiles is half the point: a `Local` address placement
//! requires the store's full read and the window's narrower one, which is the
//! bound that could not be satisfied before.

use std::sync::Arc;

use zaino_address::script_paying;
use zaino_component::{ComponentName, Lifecycle, ReachabilityProbe};
use zaino_core::Engine;
use zaino_core::routing::{Local, Passthrough, Routing, Withheld};
use zaino_core::testing::{StubNonFinalised, stub_compact_block};
use zaino_indexer::{FetchConcurrency, SourceSyncDriver, SyncTuning};
use zaino_indexes::index_set::IndexSet;
use zaino_indexes::sets::current_zaino::context_from_block;
use zaino_indexes::sets::transparent_history::TransparentHistory;
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_primitives::types::{
    Block, CompactBlock, Height, HeightRange, PreIndexCompactTx, Script, Transaction,
    TransactionId, TransparentAddress, TransparentData, TransparentInput, TransparentOutput,
    Zatoshis,
};
use zaino_runtime::{OrchestraBuilder, RunComponent, ValidatorComponent};
use zaino_service::{AddressRead, TakeSnapshot};
use zaino_source::mock::{MockChain, test_block};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::{StoreComponent, StoreReader};

/// Address history composed from the tiers; the rest set to what a deployment
/// would plausibly choose, so the table stays readable.
struct AddressLocally;

impl Routing for AddressLocally {
    type Address = Local;
    type Treestate = Passthrough;
    type Spend = Local;
    type TransactionLocation = Withheld;
}

/// The address under test, and a second one to prove answers are not shared.
///
/// The index keys by the `(script type, hash)` an address locks to, which is
/// network-independent, so these are the address crate's own test vectors: one
/// P2PKH and one P2SH, whose hashes differ.
fn addr_a() -> TransparentAddress {
    TransparentAddress::new("tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx".to_owned())
}
fn addr_b() -> TransparentAddress {
    TransparentAddress::new("t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u".to_owned())
}

/// The transaction that pays A twice and B once, below the watermark.
fn payer() -> TransactionId {
    TransactionId::from([0xA1; 32])
}
/// The transaction in the window that spends A's first output and pays A again.
fn spender() -> TransactionId {
    TransactionId::from([0xB2; 32])
}

struct Probe;
impl ReachabilityProbe for Probe {
    async fn reachable(&self) -> bool {
        true
    }
}

fn value(zats: u64) -> Zatoshis {
    Zatoshis::new(zats).expect("a valid amount")
}

fn range(start: u32, end: u32) -> HeightRange {
    HeightRange {
        start: Height::try_from(start).expect("a valid height"),
        end: Height::try_from(end).expect("a valid height"),
    }
}

/// The canonical output script paying `addr`.
fn script(addr: &TransparentAddress) -> Script {
    script_paying(addr).expect("a transparent address")
}

/// An output paying `addr`.
fn pays(addr: &TransparentAddress, zats: u64) -> TransparentOutput {
    TransparentOutput {
        value: value(zats),
        script: script(addr),
    }
}

/// The finalised store over `[0, 2]`, where height 1 mines [`payer`]: outputs 0
/// and 1 pay A (100 and 200), output 2 pays B. Finalised depth is zero, so the
/// watermark is exactly 2.
async fn indexed_store() -> StoreReader<InMemoryBackend, TransparentHistory> {
    let backend = InMemoryBackend::new();
    let mut chain = MockChain::new();
    for height in 0..=2u32 {
        let hash_byte = u8::try_from(10 + height).expect("a small height");
        let mut block: Block = test_block(height, hash_byte);
        if height == 1 {
            block.transactions = vec![Transaction {
                txid: payer(),
                transparent: TransparentData {
                    inputs: Vec::new(),
                    outputs: vec![
                        pays(&addr_a(), 100),
                        pays(&addr_a(), 200),
                        pays(&addr_b(), 300),
                    ],
                },
                sapling: Default::default(),
                orchard: Default::default(),
                ironwood: Default::default(),
            }];
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

/// The volatile window over `[3, 4]`. Height 3 mines [`spender`], which spends
/// A's first output and pays A 500.
fn window() -> StubNonFinalised {
    let spend = PreIndexCompactTx {
        txid: spender(),
        transparent_inputs: vec![TransparentInput {
            prev_txid: payer(),
            prev_index: 0,
        }],
        transparent_outputs: vec![pays(&addr_a(), 500)],
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

/// The composed engine over both tiers, under [`AddressLocally`].
async fn engine() -> impl AddressRead {
    let engine: Engine<_, _, (), AddressLocally> = Engine::new(indexed_store().await, window(), ());
    engine.snapshot().await.expect("the pin is taken")
}

/// The cross-seam case. A received 100 + 200 below the watermark and 500 in the
/// window, and the window spent the 100. Only the composition knows that: the
/// store saw no spend, and the window could not tell whose output it was.
#[tokio::test]
async fn a_receive_below_the_watermark_spent_in_the_window_leaves_the_balance() {
    let balance = engine()
        .await
        .balance(&addr_a(), range(0, 4))
        .await
        .expect("the read succeeds");
    assert_eq!(
        balance.balance,
        value(700),
        "200 unspent + 500 from the window"
    );
    assert_eq!(
        u128::from(balance.received),
        800,
        "gross receipts count the spent 100 too"
    );
}

/// The same fact through the unspent set: the spent outpoint is filtered out by
/// the window, and the window's own output is added.
#[tokio::test]
async fn the_unspent_set_drops_what_the_window_spent_and_adds_what_it_paid() {
    let unspent = engine()
        .await
        .unspent_outpoints(&addr_a())
        .await
        .expect("the read succeeds");

    let mut held: Vec<(TransactionId, u32, u64)> = unspent
        .iter()
        .map(|utxo| (utxo.txid, utxo.output_index, u64::from(utxo.satoshis)))
        .collect();
    held.sort();
    assert_eq!(
        held,
        vec![(payer(), 1, 200), (spender(), 0, 500)],
        "the payer's output 0 was spent in the window"
    );
    assert!(
        unspent.iter().all(|utxo| utxo.script == script(&addr_a())),
        "each output carries the script that pays the address"
    );
}

/// Every transaction that moved value for the address, across both halves.
#[tokio::test]
async fn transaction_ids_span_both_halves() {
    let txids = engine()
        .await
        .tx_ids(&addr_a(), range(0, 4))
        .await
        .expect("the read succeeds");
    let mut sorted = txids.clone();
    sorted.sort_by_key(|txid| <[u8; 32]>::from(*txid));
    sorted.dedup();
    assert_eq!(
        sorted,
        vec![payer(), spender()],
        "the payer below the watermark and the spender in the window"
    );
}

/// Deltas carry the receive at its own height and the spend at the spender's,
/// with the spend's magnitude taken from the output it consumed.
#[tokio::test]
async fn deltas_report_the_spend_at_its_own_height_and_value() {
    let deltas = engine()
        .await
        .deltas(&addr_a(), range(0, 4))
        .await
        .expect("the read succeeds");

    let mut reported: Vec<(u32, i64)> = deltas
        .iter()
        .map(|delta| (u32::from(delta.height), delta.satoshis.as_i64()))
        .collect();
    reported.sort();
    assert_eq!(
        reported,
        vec![(1, 100), (1, 200), (3, -100), (3, 500)],
        "two receives at height 1; at height 3 the window's spend of 100 and its payment of 500"
    );
}

/// A range entirely below the watermark never consults the window, and a
/// different address gets its own answer.
#[tokio::test]
async fn the_finalised_half_answers_alone_and_addresses_do_not_share() {
    let engine = engine().await;

    let below = engine
        .balance(&addr_a(), range(0, 2))
        .await
        .expect("the read succeeds");
    assert_eq!(
        below.balance,
        value(200),
        "the 100 is spent above the watermark, so it is not held now"
    );
    assert_eq!(
        u128::from(below.received),
        300,
        "both receives arrived here"
    );

    let other = engine
        .balance(&addr_b(), range(0, 4))
        .await
        .expect("the read succeeds");
    assert_eq!(other.balance, value(300), "B's own single output");
}
