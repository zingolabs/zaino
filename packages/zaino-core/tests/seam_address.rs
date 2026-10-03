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
use zaino_service::error::AddressReadError;
use zaino_service::{AddressRead, ChainSegment, ReadBudget, TakeSnapshot, queries};
use zaino_source::mock::{MockChain, test_block};
use zaino_source::{RetryPolicy, ValidatorClient};
use zaino_store::StoreReader;
use zaino_store_service::StoreComponent;

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
        .balance(&addr_a(), range(0, 4), &mut ReadBudget::for_request())
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
        .unspent_outpoints(&addr_a(), &mut ReadBudget::for_request())
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
    let located = engine()
        .await
        .tx_ids(&addr_a(), range(0, 4), &mut ReadBudget::for_request())
        .await
        .expect("the read succeeds");
    // The read pairs each txid with where it touched the address; here only the
    // set of transactions is under test.
    let mut sorted: Vec<TransactionId> = located.into_iter().map(|(_, _, txid)| txid).collect();
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
        .deltas(&addr_a(), range(0, 4), &mut ReadBudget::for_request())
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

// --- multi-address merge, queries layer, real local engine -------------------
//
// zcashd's `getaddresstxids`/`getaddressutxos` build one set across all requested
// addresses, globally ordered by height — not one list per address concatenated.
// The local store read is height-sorted only *within* one address, so the merge
// must interleave. This fixture puts A, then B, then A at ascending heights so a
// grouped-by-address answer (A's txs, then B's) is distinguishable from the
// correct height-ordered one.

/// A paid at height 1.
fn merge_a1() -> TransactionId {
    TransactionId::from([0xC1; 32])
}
/// B paid at height 2 — between A's two transactions.
fn merge_b2() -> TransactionId {
    TransactionId::from([0xC2; 32])
}
/// A paid again at height 3.
fn merge_a3() -> TransactionId {
    TransactionId::from([0xC3; 32])
}

/// A finalised store over `[0, 4]`: height 1 pays A, height 2 pays B, height 3
/// pays A again, each in its own transaction. Finalised depth is zero, so the
/// whole span is below the watermark and the window is empty.
async fn interleaved_store() -> StoreReader<InMemoryBackend, TransparentHistory> {
    let backend = InMemoryBackend::new();
    let mut chain = MockChain::new();
    for height in 0..=4u32 {
        let hash_byte = u8::try_from(20 + height).expect("a small height");
        let mut block: Block = test_block(height, hash_byte);
        let paid = match height {
            1 => Some((merge_a1(), &addr_a(), 100u64)),
            2 => Some((merge_b2(), &addr_b(), 200)),
            3 => Some((merge_a3(), &addr_a(), 300)),
            _ => None,
        };
        if let Some((txid, addr, zats)) = paid {
            block.transactions = vec![Transaction {
                txid,
                transparent: TransparentData {
                    inputs: Vec::new(),
                    outputs: vec![pays(addr, zats)],
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

/// The composed engine over the interleaved store and an empty window.
async fn interleaved_engine() -> impl AddressRead + ChainSegment {
    let engine: Engine<_, _, (), AddressLocally> =
        Engine::new(interleaved_store().await, StubNonFinalised::empty(), ());
    engine.snapshot().await.expect("the pin is taken")
}

/// `getaddresstxids` over `[A, B]` returns one globally height-ordered list:
/// A@1, B@2, A@3. A grouped-by-address answer would be A@1, A@3, B@2 and fails.
#[tokio::test]
async fn address_txids_merge_addresses_globally_by_height() {
    let snapshot = interleaved_engine().await;
    let txids = queries::address_txids(&snapshot, &[addr_a(), addr_b()], None, None)
        .await
        .expect("the read succeeds");
    assert_eq!(
        txids,
        vec![merge_a1(), merge_b2(), merge_a3()],
        "the union is ordered by height across addresses, not grouped by address"
    );
}

/// `getaddressutxos` over `[A, B]` is likewise height-ordered across addresses:
/// the three unspent outputs come back at heights 1, 2, 3 in that order.
#[tokio::test]
async fn address_utxos_merge_addresses_globally_by_height() {
    let snapshot = interleaved_engine().await;
    let utxos = queries::address_utxos(&snapshot, &[addr_a(), addr_b()])
        .await
        .expect("the read succeeds");
    let located: Vec<(u32, TransactionId)> = utxos
        .iter()
        .map(|utxo| (u32::from(utxo.height), utxo.txid))
        .collect();
    assert_eq!(
        located,
        vec![(1, merge_a1()), (2, merge_b2()), (3, merge_a3()),],
        "the unspent set is ordered by height across addresses"
    );
}

// --- same-height tie-break, in-block position order --------------------------
//
// zcashd keys `getaddresstxids` on `(height, txindex, txid)` (rpc/misc.cpp): for
// transactions sharing a height, `txindex` — the position in the block — orders
// them, and the txid's *display-hex* string is only the final tie-break. These
// two txids are mined in an order that disagrees with display-hex order, so the
// result pins the position key and would fail a regression that ordered
// same-height txids by the txid.

/// Earlier in the block (position 0 below), but *larger* by display hex
/// (`ff00…00`): its high byte sits last in storage and first on the wire.
fn tx_block_first() -> TransactionId {
    let mut bytes = [0u8; 32];
    bytes[31] = 0xFF;
    TransactionId::from(bytes)
}
/// Later in the block (position 1 below), but *smaller* by display hex
/// (`00…00ff`): the mirror of [`tx_block_first`]. A result that ordered by the
/// txid would return this one first; ordering by block position returns it second.
fn tx_block_second() -> TransactionId {
    let mut bytes = [0u8; 32];
    bytes[0] = 0xFF;
    TransactionId::from(bytes)
}

/// A finalised store over `[0, 2]` whose height-1 block mines both
/// [`tx_block_first`] and [`tx_block_second`] paying A, so a `tx_ids` read returns
/// two txids at one shared height for the tie-break to order.
async fn same_height_store() -> StoreReader<InMemoryBackend, TransparentHistory> {
    let backend = InMemoryBackend::new();
    let mut chain = MockChain::new();
    for height in 0..=2u32 {
        let hash_byte = u8::try_from(30 + height).expect("a small height");
        let mut block: Block = test_block(height, hash_byte);
        if height == 1 {
            let pay = |txid: TransactionId, zats: u64| Transaction {
                txid,
                transparent: TransparentData {
                    inputs: Vec::new(),
                    outputs: vec![pays(&addr_a(), zats)],
                },
                sapling: Default::default(),
                orchard: Default::default(),
                ironwood: Default::default(),
            };
            // Block order (position 0 then 1) is the reverse of display-hex order,
            // so a result in block order proves position — not the txid — is the
            // same-height key.
            block.transactions = vec![pay(tx_block_first(), 100), pay(tx_block_second(), 200)];
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

/// The composed engine over the same-height store and an empty window.
async fn same_height_engine() -> impl AddressRead + ChainSegment {
    let engine: Engine<_, _, (), AddressLocally> =
        Engine::new(same_height_store().await, StubNonFinalised::empty(), ());
    engine.snapshot().await.expect("the pin is taken")
}

/// Two txids at one height come back in **block position** order, not txid order.
/// The fixture mines them in an order that disagrees with display-hex order, so
/// this pins zcashd's `(height, position, txid)` key and fails a regression that
/// ordered same-height txids by the txid alone.
#[tokio::test]
async fn address_txids_break_same_height_ties_by_block_position() {
    let snapshot = same_height_engine().await;
    let txids = queries::address_txids(&snapshot, &[addr_a()], None, None)
        .await
        .expect("the read succeeds");
    assert_eq!(
        txids,
        vec![tx_block_first(), tx_block_second()],
        "same-height txids order by their position in the block, not by the txid"
    );
}

/// A range entirely below the watermark never consults the window, and a
/// different address gets its own answer.
#[tokio::test]
async fn the_finalised_half_answers_alone_and_addresses_do_not_share() {
    let engine = engine().await;

    let below = engine
        .balance(&addr_a(), range(0, 2), &mut ReadBudget::for_request())
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
        .balance(&addr_b(), range(0, 4), &mut ReadBudget::for_request())
        .await
        .expect("the read succeeds");
    assert_eq!(other.balance, value(300), "B's own single output");
}

// --- request-scoped budget, across addresses --------------------------------
//
// The ceiling is per request, not per address: one budget threaded through every
// address a query reads bounds their combined entries. This store pays two
// addresses two receives each (window empty, so one store scan per address), so a
// budget that each address fits under alone is overrun by the two together.

/// A paid at heights 1 and 2; B paid at heights 3 and 4 — two receives each.
fn budget_store_tx(seed: u8) -> TransactionId {
    TransactionId::from([seed; 32])
}

/// A finalised store over `[0, 4]`: A is paid at heights 1 and 2, B at heights 3
/// and 4, each payment its own transaction. Finalised depth is zero, so the whole
/// span is below the watermark and the window is empty.
async fn budget_store() -> StoreReader<InMemoryBackend, TransparentHistory> {
    let backend = InMemoryBackend::new();
    let mut chain = MockChain::new();
    for height in 0..=4u32 {
        let hash_byte = u8::try_from(40 + height).expect("a small height");
        let mut block: Block = test_block(height, hash_byte);
        let paid = match height {
            1 => Some((budget_store_tx(0xD1), &addr_a(), 100u64)),
            2 => Some((budget_store_tx(0xD2), &addr_a(), 200)),
            3 => Some((budget_store_tx(0xD3), &addr_b(), 300)),
            4 => Some((budget_store_tx(0xD4), &addr_b(), 400)),
            _ => None,
        };
        if let Some((txid, addr, zats)) = paid {
            block.transactions = vec![Transaction {
                txid,
                transparent: TransparentData {
                    inputs: Vec::new(),
                    outputs: vec![pays(addr, zats)],
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

/// The composed engine over the two-address budget store and an empty window.
async fn budget_engine() -> impl AddressRead + ChainSegment {
    let engine: Engine<_, _, (), AddressLocally> =
        Engine::new(budget_store().await, StubNonFinalised::empty(), ());
    engine.snapshot().await.expect("the pin is taken")
}

/// One budget threaded through two addresses bounds them together. Each address
/// holds two receives; under a budget of three each fits alone, but the second
/// read overruns the shared budget and is refused with `TooLarge`. A per-address
/// ceiling could not catch this — the regression the request-scoped budget fixes.
#[tokio::test]
async fn a_multi_address_request_is_bounded_as_a_whole() {
    let snapshot = budget_engine().await;
    let whole = range(0, 4);

    // Each address alone is within a budget of three (two receives < three).
    for addr in [addr_a(), addr_b()] {
        let alone = snapshot
            .tx_ids(&addr, whole, &mut ReadBudget::with_limit(3))
            .await
            .expect("one address of two receives fits under three");
        assert_eq!(alone.len(), 2, "both receives of the address come back");
    }

    // Shared across both, the two four receives overrun the budget of three: the
    // first address is served, the second refused.
    let mut budget = ReadBudget::with_limit(3);
    let first = snapshot
        .tx_ids(&addr_a(), whole, &mut budget)
        .await
        .expect("the first address is within budget");
    assert_eq!(first.len(), 2);
    let refused = snapshot
        .tx_ids(&addr_b(), whole, &mut budget)
        .await
        .expect_err("the second address overruns the shared budget");
    assert!(
        matches!(refused, AddressReadError::TooLarge { limit: 3, .. }),
        "got {refused:?}"
    );
}

// --- same-height tie-break across the seam, window positions -----------------
//
// The live regression: for an address whose transactions straddle one height in
// the non-finalised window, the window path reported no in-block position, so
// same-height entries fell through to the display-hex tie-break instead of
// ordering by block position as zcashd does. The case that caught it had the
// address's *spend* at a later block position than a *receive* at the same
// height, with the spend's display-hex smaller — so the hex tie-break put the
// spend first, the reverse of zcashd's block-position order.
//
// This fixture reproduces it in the window: at height 3, a receive sits at block
// position 1 and a spend of a finalised output at position 3, and the spend's
// txid is smaller in display hex. The correct answer orders them by position
// (receive then spend); the pre-fix window ordering returned the reverse.

/// A filler transaction at a given block position, touching neither address.
fn window_filler(seed: u8) -> PreIndexCompactTx {
    PreIndexCompactTx {
        txid: TransactionId::from([seed; 32]),
        transparent_inputs: Vec::new(),
        transparent_outputs: Vec::new(),
        sapling_nullifiers: Vec::new(),
        sapling_outputs: Vec::new(),
        orchard_actions: Vec::new(),
        ironwood_actions: Vec::new(),
    }
}

/// The window for the position test: height 3 holds four transactions, with a
/// receive to A at position 1 ([`tx_block_first`], larger in display hex) and a
/// spend of A's finalised output at position 3 ([`tx_block_second`], smaller in
/// display hex), fillers at positions 0 and 2. Block-position order is therefore
/// the reverse of display-hex order.
fn window_positions() -> StubNonFinalised {
    let receive = PreIndexCompactTx {
        txid: tx_block_first(),
        transparent_inputs: Vec::new(),
        transparent_outputs: vec![pays(&addr_a(), 500)],
        sapling_nullifiers: Vec::new(),
        sapling_outputs: Vec::new(),
        orchard_actions: Vec::new(),
        ironwood_actions: Vec::new(),
    };
    let spend = PreIndexCompactTx {
        txid: tx_block_second(),
        transparent_inputs: vec![TransparentInput {
            prev_txid: payer(),
            prev_index: 0,
        }],
        transparent_outputs: Vec::new(),
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
    // Positions 0..=3: filler, receive, filler, spend.
    blocks[0].transactions = vec![window_filler(0x01), receive, window_filler(0x02), spend];
    StubNonFinalised::from_blocks(blocks)
}

/// The composed engine over the finalised store and the position window.
async fn window_positions_engine() -> impl AddressRead + ChainSegment {
    let engine: Engine<_, _, (), AddressLocally> =
        Engine::new(indexed_store().await, window_positions(), ());
    engine.snapshot().await.expect("the pin is taken")
}

/// Same-height entries from the non-finalised window order by block position, not
/// by display-hex txid. The receive is at position 1 and the spend at position 3,
/// so the answer is receive-then-spend; the pre-fix window path reported no
/// position, which collapsed to the display-hex order and returned spend-then-
/// receive (the spend's txid being smaller in hex).
#[tokio::test]
async fn window_txids_break_same_height_ties_by_block_position() {
    let snapshot = window_positions_engine().await;
    let txids = queries::address_txids(&snapshot, &[addr_a()], None, None)
        .await
        .expect("the read succeeds");
    assert_eq!(
        txids,
        vec![payer(), tx_block_first(), tx_block_second()],
        "the finalised payer at height 1, then the window's receive (position 1) \
         before its spend (position 3) — block-position order, not display-hex order"
    );
}
