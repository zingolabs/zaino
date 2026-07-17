//! Conformance test-kit for implementations of the port.
//!
//! Every implementation must pass every case here (decision 8 of the
//! design review): the kit is the executable half of the contract.
//! Engine adapter crates enable this crate's `testing` feature and call
//! these functions from their own test suites against a ready engine.
//!
//! Each case asserts an invariant the contract promises, phrased so no
//! oracle beyond the port itself is needed; a violation panics with a
//! message naming the broken promise. Liveness promises are held to a
//! generous internal deadline, so a stream that wrongly never yields or
//! never ends fails loudly instead of hanging the suite; the deadline
//! runs on tokio's timer — the kit's one runtime assumption — so run
//! the cases inside a tokio runtime (`#[tokio::test]` suffices).
//!
//! What the kit cannot fabricate, the caller supplies: some cases take
//! the implementation-specific way to move the chain, or handles (a
//! transparent address with history, a mined txid, an in-view
//! outpoint) that only the test harness can know.

use std::future::Future;
use std::pin::{pin, Pin};
use std::time::Duration;

use core::fmt;

use futures::{Stream, StreamExt};
use zaino_primitives::types::{BlockHash, Height, TransactionHash, TransparentAddress};

use crate::block_id::BlockId;
use crate::block_locator::BlockLocator;
use crate::broadcast_transaction::{BroadcastTransaction, BroadcastTransactionError};
use crate::driving_port::DrivingPort;
use crate::error::{FailureClass, PortError};
use crate::find_fork_point::FindForkPoint;
use crate::get_address_transaction_ids::GetAddressTransactionIds;
use crate::get_address_unspent_outpoints::GetAddressUnspentOutpoints;
use crate::get_health::{GetHealth, Health};
use crate::get_mined_transaction::GetMinedTransaction;
use crate::get_outpoint_spend_status::GetOutpointSpendStatus;
use crate::get_raw_block::GetRawBlock;
use crate::get_raw_block_header::GetRawBlockHeader;
use crate::get_reported_upgrades::GetReportedUpgrades;
use crate::get_transaction_status::GetTransactionStatus;
use crate::get_treestate::GetTreestate;
use crate::hash_for_height::GetHashForHeight;
use crate::height_for_hash::GetHeightForHash;
use crate::mempool_transaction::MempoolTransaction;
use crate::outpoint::Outpoint;
use crate::pinned_tip::GetPinnedTip;
use crate::raw::RawTransaction;
use crate::reported_upgrade::UpgradeStatus;
use crate::stream_raw_blocks::StreamRawBlocks;
use crate::subscribe_to_mempool::SubscribeToMempool;
use crate::subscribe_to_tip_changes::SubscribeToTipChanges;
use crate::take_snapshot::TakeSnapshot;
use crate::transaction_status::TransactionStatus;

/// How long the kit waits for a promised event before declaring the
/// promise broken. Generous: a conforming engine answers in
/// milliseconds, so only a violation ever reaches the deadline.
const PROMISE_DEADLINE: Duration = Duration::from_secs(30);

/// Await `future`, panicking with the broken `promise` when the
/// deadline passes. A violated liveness promise must fail loudly with
/// the promise's name, never hang the suite into an opaque timeout.
async fn expect_within<T>(future: impl Future<Output = T>, promise: &str) -> T {
    match tokio::time::timeout(PROMISE_DEADLINE, future).await {
        Ok(value) => value,
        Err(_) => panic!("{promise} (nothing arrived within {PROMISE_DEADLINE:?})"),
    }
}

/// Take a snapshot from a port the running case requires ready.
async fn ready_snapshot<P: TakeSnapshot>(port: &P) -> P::Snapshot {
    port.take_snapshot()
        .await
        .expect("port under conformance must be ready")
}

/// The next delivery of `txid` on the mempool stream, skipping
/// unrelated entries; panics with `promise` when the stream ends or
/// the delivery never arrives.
async fn next_delivery_of<S>(
    mut mempool: Pin<&mut S>,
    txid: TransactionHash,
    promise: &str,
) -> MempoolTransaction
where
    S: Stream<Item = MempoolTransaction>,
{
    expect_within(
        async {
            loop {
                let entry = mempool.next().await.expect(promise);
                if entry.txid == txid {
                    return entry;
                }
            }
        },
        promise,
    )
    .await
}

/// Assert that a post-shutdown operation failed with a fatal backend
/// error — the ShutDown contract's answer for every operation on a
/// dead port, domain rejections included.
fn assert_fatal_backend<T, E: fmt::Debug + fmt::Display>(
    result: Result<T, PortError<E>>,
    operation: &str,
) {
    match result {
        Err(PortError::Backend(failure)) if failure.class == FailureClass::Fatal => {}
        Err(other) => panic!(
            "after shutdown, {operation} must fail with a fatal backend error, got: {other:?}"
        ),
        Ok(_) => {
            panic!("after shutdown, {operation} must fail with a fatal backend error, got success")
        }
    }
}

/// A hash almost surely absent from the pinned chain: `base` with one
/// byte flipped. The contract treats any hash off the pinned chain as
/// absent, and a collision with a real block is negligible.
fn off_chain_hash(base: BlockHash) -> BlockHash {
    let mut bytes = <[u8; 32]>::from(base);
    bytes[16] ^= 0xFF;
    bytes.into()
}

/// A snapshot agrees with itself: the pinned tip resolves through both
/// identity lookups.
///
/// Precondition: the port is ready (`take_snapshot` succeeds).
pub async fn snapshot_is_self_consistent<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();

    let hash = snapshot
        .get_hash_for_height(tip.height)
        .await
        .expect("lookup at the pinned tip must not fail");
    assert_eq!(
        hash,
        Some(tip.hash),
        "the hash at the pinned tip's height must be the pinned tip's hash"
    );

    let height = snapshot
        .get_height_for_hash(tip.hash)
        .await
        .expect("lookup of the pinned tip must not fail");
    assert_eq!(
        height,
        Some(tip.height),
        "the height of the pinned tip's hash must be the pinned tip's height"
    );
}

/// Absence is an answer: heights beyond the pinned tip and hashes not
/// on the pinned chain read `None`, never an error.
///
/// Precondition: the port is ready. The "unknown" hash is the pinned
/// tip's hash with one byte flipped; the contract treats a hash off the
/// pinned chain as absent, and a collision with a real block is
/// negligible.
pub async fn absent_blocks_read_none<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();

    if let Some(beyond_tip) = tip.height.checked_add(1) {
        let hash = snapshot
            .get_hash_for_height(beyond_tip)
            .await
            .expect("a beyond-tip lookup must not fail");
        assert_eq!(hash, None, "a height beyond the pinned tip must read None");
    }

    let height = snapshot
        .get_height_for_hash(off_chain_hash(tip.hash))
        .await
        .expect("an unknown-hash lookup must not fail");
    assert_eq!(height, None, "a hash off the pinned chain must read None");
}

/// A locator holding only the pinned tip forks at the pinned tip.
///
/// Precondition: the port is ready.
pub async fn fork_point_of_the_tip_locator_is_the_tip<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();

    let locator = BlockLocator::new(vec![tip]).expect("a single-entry locator is well-formed");
    let fork = snapshot
        .find_fork_point(&locator)
        .await
        .expect("fork-point detection must not fail");
    assert_eq!(
        fork,
        Some(tip),
        "a locator holding only the pinned tip must fork at the pinned tip"
    );
}

/// Among several matches, the fork point is the highest one.
///
/// Preconditions: the port is ready, the pinned chain extends beyond
/// genesis, and genesis is in the pinned view.
pub async fn fork_point_prefers_the_highest_match<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();
    assert!(
        tip.height > Height::GENESIS,
        "conformance precondition: the pinned chain must extend beyond genesis"
    );
    let genesis = genesis_of(&snapshot).await;

    let locator =
        BlockLocator::new(vec![tip, genesis]).expect("descending entries are well-formed");
    let fork = snapshot
        .find_fork_point(&locator)
        .await
        .expect("fork-point detection must not fail");
    assert_eq!(
        fork,
        Some(tip),
        "with both the tip and genesis on the chain, the fork point must be the tip"
    );
}

/// Locator entries off the pinned chain are skipped, not matched.
///
/// Preconditions: the port is ready, the pinned chain extends beyond
/// genesis, and genesis is in the pinned view.
pub async fn fork_point_skips_entries_off_the_chain<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();
    assert!(
        tip.height > Height::GENESIS,
        "conformance precondition: the pinned chain must extend beyond genesis"
    );
    let genesis = genesis_of(&snapshot).await;

    let stranger = BlockId {
        height: tip.height,
        hash: off_chain_hash(tip.hash),
    };
    let locator =
        BlockLocator::new(vec![stranger, genesis]).expect("descending entries are well-formed");
    let fork = snapshot
        .find_fork_point(&locator)
        .await
        .expect("fork-point detection must not fail");
    assert_eq!(
        fork,
        Some(genesis),
        "an entry off the pinned chain must be skipped in favor of a real match"
    );
}

/// A locator sharing no block with the pinned chain answers `None`.
///
/// Precondition: the port is ready.
pub async fn fork_point_is_none_when_no_entry_matches<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();

    let stranger = BlockId {
        height: tip.height,
        hash: off_chain_hash(tip.hash),
    };
    let locator = BlockLocator::new(vec![stranger]).expect("a single-entry locator is well-formed");
    let fork = snapshot
        .find_fork_point(&locator)
        .await
        .expect("fork-point detection must not fail");
    assert_eq!(
        fork, None,
        "a locator sharing no block with the pinned chain must answer None"
    );
}

/// The full-range stream covers the pinned view exactly: one block per
/// height from genesis to the pinned tip, ascending, none empty.
///
/// Preconditions: the port is ready and the pinned tip is below the
/// protocol maximum height.
pub async fn stream_covers_the_pinned_range<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();
    let beyond_tip = tip
        .height
        .checked_add(1)
        .expect("conformance precondition: pinned tip below the protocol maximum");

    let items: Vec<_> = snapshot
        .stream_raw_blocks(Height::GENESIS..beyond_tip)
        .collect()
        .await;
    assert_eq!(
        items.len() as u64,
        u64::from(tip.height) + 1,
        "the full-range stream must yield one block per height up to the pinned tip"
    );

    let mut expected = Height::GENESIS;
    for item in items {
        let (id, raw) = item.expect("a streamed block must not be an error");
        assert_eq!(
            id.height, expected,
            "streamed blocks must ascend by height without gaps"
        );
        assert!(
            !raw.as_slice().is_empty(),
            "a consensus-serialized block is never empty"
        );
        expected = expected
            .checked_add(1)
            .expect("heights stay below the protocol maximum");
    }
}

/// A range reaching beyond the pinned tip is clamped to the pinned
/// view; a range past it entirely yields nothing.
///
/// Preconditions: the port is ready and the pinned tip is at least
/// four blocks below the protocol maximum height.
pub async fn stream_clamps_to_the_pinned_view<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();
    let beyond_tip = tip
        .height
        .checked_add(1)
        .expect("conformance precondition: pinned tip below the protocol maximum");
    let far_beyond = tip
        .height
        .checked_add(4)
        .expect("conformance precondition: pinned tip well below the protocol maximum");

    let straddling: Vec<_> = snapshot
        .stream_raw_blocks(tip.height..far_beyond)
        .collect()
        .await;
    assert_eq!(
        straddling.len(),
        1,
        "a range straddling the pinned tip must be clamped to it"
    );
    let (id, _) = straddling
        .into_iter()
        .next()
        .expect("one item was just asserted")
        .expect("a streamed block must not be an error");
    assert_eq!(id, tip, "the clamped stream must end at the pinned tip");

    let past: Vec<_> = snapshot
        .stream_raw_blocks(beyond_tip..far_beyond)
        .collect()
        .await;
    assert!(
        past.is_empty(),
        "a range entirely beyond the pinned tip must yield nothing"
    );
}

/// The point reads agree with the stream: every streamed block reads
/// back identically through GetRawBlock, and its header through
/// GetRawBlockHeader is the prefix of the block's serialization.
///
/// Preconditions: the port is ready and the pinned tip is below the
/// protocol maximum height.
pub async fn raw_reads_agree_with_the_stream<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();
    let beyond_tip = tip
        .height
        .checked_add(1)
        .expect("conformance precondition: pinned tip below the protocol maximum");

    let mut stream = pin!(snapshot.stream_raw_blocks(Height::GENESIS..beyond_tip));
    while let Some(item) = stream.next().await {
        let (id, streamed) = item.expect("a streamed block must not be an error");

        let read = snapshot
            .get_raw_block(id.height)
            .await
            .expect("an in-view block read must not fail")
            .expect("a streamed block must also read point-wise");
        assert_eq!(
            read, streamed,
            "GetRawBlock must return the bytes the stream yielded"
        );

        let header = snapshot
            .get_raw_block_header(id.height)
            .await
            .expect("an in-view header read must not fail")
            .expect("a streamed block's header must also read point-wise");
        assert!(
            streamed.as_slice().starts_with(header.as_slice()),
            "a block's header must be the prefix of the block's serialization"
        );
    }
}

/// Payload reads beyond the pinned tip answer `None`, never an error.
///
/// Preconditions: the port is ready and the pinned tip is below the
/// protocol maximum height.
pub async fn absent_payloads_read_none<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let beyond_tip = snapshot
        .get_pinned_tip()
        .height
        .checked_add(1)
        .expect("conformance precondition: pinned tip below the protocol maximum");

    let block = snapshot
        .get_raw_block(beyond_tip)
        .await
        .expect("a beyond-tip block read must not fail");
    assert_eq!(block, None, "a block beyond the pinned tip must read None");

    let header = snapshot
        .get_raw_block_header(beyond_tip)
        .await
        .expect("a beyond-tip header read must not fail");
    assert_eq!(
        header, None,
        "a header beyond the pinned tip must read None"
    );
}

/// A txid the pinned view does not know reads `None` and `Unknown`,
/// consistently across both transaction capabilities.
///
/// Precondition: the port is ready. The fabricated txid collides with
/// a real transaction with negligible probability.
pub async fn unknown_transactions_read_none_and_unknown<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;

    let mut bytes = <[u8; 32]>::from(snapshot.get_pinned_tip().hash);
    bytes[8] ^= 0xFF;
    let stranger = TransactionHash::from(bytes);

    let mined = snapshot
        .get_mined_transaction(stranger)
        .await
        .expect("an unknown-txid read must not fail");
    assert_eq!(mined, None, "an unknown txid must read None");

    let status = snapshot
        .get_transaction_status(stranger)
        .await
        .expect("an unknown-txid status must not fail");
    assert_eq!(
        status,
        TransactionStatus::Unknown,
        "an unknown txid's status must be Unknown, agreeing with the None read"
    );
}

/// The two transaction capabilities agree on a mined transaction: the
/// status names exactly the block the payload read reports, and that
/// block sits on the pinned best chain.
///
/// The caller supplies `mined_txid` — a txid mined in the current best
/// chain, which the kit cannot learn from raw block bytes on its own.
///
/// Preconditions: the port is ready and `mined_txid` is mined in the
/// pinned view.
pub async fn mined_transactions_agree_across_capabilities<P: TakeSnapshot>(
    port: &P,
    mined_txid: TransactionHash,
) {
    let snapshot = ready_snapshot(port).await;

    let mined = snapshot
        .get_mined_transaction(mined_txid)
        .await
        .expect("a mined-transaction read must not fail")
        .expect("conformance precondition: the supplied txid must be mined in the pinned view");
    assert!(
        !mined.raw.as_slice().is_empty(),
        "a mined transaction carries its serialization"
    );

    let status = snapshot
        .get_transaction_status(mined_txid)
        .await
        .expect("a status read must not fail");
    assert_eq!(
        status,
        TransactionStatus::MinedAt(mined.mined_at),
        "the status must name the block the mined-transaction read reports"
    );

    let hash = snapshot
        .get_hash_for_height(mined.mined_at.height)
        .await
        .expect("an in-view lookup must not fail");
    assert_eq!(
        hash,
        Some(mined.mined_at.hash),
        "the mined-at block must sit on the pinned best chain"
    );
}

/// Every in-view height has a treestate, pinned to the right block;
/// what varies per pool is only whether the frontier is present, and
/// absence means an empty tree, never an error (zcash/zallet#455).
///
/// Preconditions: the port is ready and genesis is in the pinned view.
pub async fn treestates_exist_at_every_in_view_height<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();
    let genesis = genesis_of(&snapshot).await;

    for block in [genesis, tip] {
        let treestate = snapshot
            .get_treestate(block.height)
            .await
            .expect("an in-view treestate read must not fail")
            .expect("every in-view height must have a treestate");
        assert_eq!(
            treestate.at, block,
            "a treestate must be pinned to the block at its height"
        );
    }
}

/// A treestate beyond the pinned tip reads `None`, never an error.
///
/// Preconditions: the port is ready and the pinned tip is below the
/// protocol maximum height.
pub async fn absent_treestates_read_none<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let beyond_tip = snapshot
        .get_pinned_tip()
        .height
        .checked_add(1)
        .expect("conformance precondition: pinned tip below the protocol maximum");

    let treestate = snapshot
        .get_treestate(beyond_tip)
        .await
        .expect("a beyond-tip treestate read must not fail");
    assert_eq!(
        treestate, None,
        "a treestate beyond the pinned tip must read None"
    );
}

/// An outpoint no transaction in the pinned view created reads
/// `None`, never an error and never `Unspent`.
///
/// Precondition: the port is ready. The address capabilities have no
/// standalone kit case — the kit cannot fabricate a meaningfully valid
/// transparent address without network knowledge — but the pinning
/// case exercises them across a reorg through caller-supplied handles.
pub async fn unknown_outpoints_read_none<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;

    let mut bytes = <[u8; 32]>::from(snapshot.get_pinned_tip().hash);
    bytes[24] ^= 0xFF;
    let stranger = Outpoint {
        txid: TransactionHash::from(bytes),
        index: 0,
    };

    let status = snapshot
        .get_outpoint_spend_status(stranger)
        .await
        .expect("an unknown-outpoint read must not fail");
    assert_eq!(
        status, None,
        "an outpoint no in-view transaction created must read None, not Unspent"
    );
}

/// A fresh subscription yields the current tip as its first event,
/// with no chain movement required.
///
/// Preconditions: the port is ready and the chain is quiescent while
/// the case runs.
pub async fn subscription_yields_the_current_tip_first<P>(port: &P)
where
    P: TakeSnapshot + SubscribeToTipChanges,
{
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();

    let mut events = pin!(port.subscribe_to_tip_changes());
    let first = expect_within(
        events.next(),
        "a fresh subscription must deliver the current tip first",
    )
    .await
    .expect("the subscription must yield while the port lives");
    assert_eq!(
        first, tip,
        "a fresh subscription must deliver the current tip first"
    );
}

/// Chain movement surfaces as a tip event carrying the new tip.
///
/// The caller supplies `move_chain`, the implementation-specific way
/// to move the chain once — the mock's controller scripting an
/// advance, an engine harness mining a regtest block. Events may
/// coalesce, so with exactly one movement the next event is the new
/// tip.
///
/// Preconditions: the port is ready, and nothing but `move_chain`
/// moves the chain while the case runs.
pub async fn tip_events_follow_chain_movement<P>(port: &P, move_chain: impl Future<Output = ()>)
where
    P: TakeSnapshot + SubscribeToTipChanges,
{
    let mut events = pin!(port.subscribe_to_tip_changes());
    let first = expect_within(
        events.next(),
        "a fresh subscription must deliver the current tip first",
    )
    .await
    .expect("the subscription must yield while the port lives");

    move_chain.await;

    let second = expect_within(
        events.next(),
        "a chain movement must surface as a tip event",
    )
    .await
    .expect("the subscription must yield while the port lives");
    assert_ne!(second, first, "the event must carry a different tip");

    let now = ready_snapshot(port).await.get_pinned_tip();
    assert_eq!(second, now, "the event must carry the chain's new tip");
}

/// Tip events may coalesce under load, but the latest tip is always
/// eventually delivered — a subscriber that missed intermediate
/// movements must still land on the chain's current tip, never on a
/// stale one with nothing to follow.
///
/// The caller supplies `move_chain`, the implementation-specific way
/// to move the chain once; the case invokes it several times.
///
/// Preconditions: the port is ready, and nothing but `move_chain`
/// moves the chain while the case runs.
pub async fn tip_events_coalesce_to_the_latest<P, F, Fut>(port: &P, mut move_chain: F)
where
    P: TakeSnapshot + SubscribeToTipChanges,
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut events = pin!(port.subscribe_to_tip_changes());
    expect_within(
        events.next(),
        "a fresh subscription must deliver the current tip first",
    )
    .await
    .expect("the subscription must yield while the port lives");

    for _ in 0..3 {
        move_chain().await;
    }
    let latest = ready_snapshot(port).await.get_pinned_tip();

    expect_within(
        async {
            loop {
                let event = events
                    .next()
                    .await
                    .expect("the subscription must yield while the port lives");
                if event == latest {
                    return;
                }
            }
        },
        "under coalescing, the latest tip must always eventually be delivered",
    )
    .await;
}

/// Decision 5 of the design review — and ADR 0003 — executable: a
/// snapshot keeps serving its pinned view, through every one of its
/// thirteen capabilities, while any clone of it lives, even across a
/// reorg that replaces the blocks it pinned.
///
/// The caller supplies `reorg_chain`, the implementation-specific way
/// to reorganize (or at least move) the chain, plus the handles the
/// kit cannot fabricate: a transparent `address` with history in the
/// pre-movement view, an `outpoint` some in-view transaction created,
/// and a `mined_txid` mined in the pre-movement view. The case records
/// every capability's answer, drops the original keeping only a clone,
/// moves the chain, verifies the port's fresh view really moved, and
/// then re-reads everything through the surviving clone.
///
/// Preconditions: the port is ready, the pinned tip is below the
/// protocol maximum height, the handles are in the pre-movement view,
/// and nothing but `reorg_chain` moves the chain while the case runs.
pub async fn snapshots_stay_pinned_across_chain_movement<P: TakeSnapshot>(
    port: &P,
    reorg_chain: impl Future<Output = ()>,
    address: &TransparentAddress,
    outpoint: Outpoint,
    mined_txid: TransactionHash,
) {
    let snapshot = ready_snapshot(port).await;
    let tip = snapshot.get_pinned_tip();
    let beyond_tip = tip
        .height
        .checked_add(1)
        .expect("conformance precondition: pinned tip below the protocol maximum");
    let full_range = Height::GENESIS..beyond_tip;

    let before_stream: Vec<_> = snapshot
        .stream_raw_blocks(full_range.clone())
        .map(|item| item.expect("a streamed block must not be an error"))
        .collect()
        .await;
    let before_block = snapshot
        .get_raw_block(tip.height)
        .await
        .expect("an in-view block read must not fail");
    let before_header = snapshot
        .get_raw_block_header(tip.height)
        .await
        .expect("an in-view header read must not fail");
    let before_treestate = snapshot
        .get_treestate(tip.height)
        .await
        .expect("an in-view treestate read must not fail");
    let before_mined = snapshot
        .get_mined_transaction(mined_txid)
        .await
        .expect("a mined-transaction read must not fail");
    assert!(
        before_mined.is_some(),
        "conformance precondition: the supplied txid must be mined in the pre-movement view"
    );
    let before_status = snapshot
        .get_transaction_status(mined_txid)
        .await
        .expect("a status read must not fail");
    let before_unspent = snapshot
        .get_address_unspent_outpoints(address, full_range.clone())
        .await
        .expect("an address query must not fail");
    let before_txids = snapshot
        .get_address_transaction_ids(address, full_range.clone())
        .await
        .expect("an address-txids query must not fail");
    let before_spend = snapshot
        .get_outpoint_spend_status(outpoint)
        .await
        .expect("a spend-status read must not fail");
    assert!(
        before_spend.is_some(),
        "conformance precondition: the supplied outpoint must be known to the pre-movement view"
    );

    let clone = snapshot.clone();
    drop(snapshot);

    reorg_chain.await;

    let fresh = ready_snapshot(port).await;
    assert_ne!(
        fresh.get_pinned_tip(),
        tip,
        "conformance precondition: reorg_chain must move the chain"
    );

    assert_eq!(
        clone.get_pinned_tip(),
        tip,
        "the pinned tip must survive the reorg"
    );
    let hash = clone
        .get_hash_for_height(tip.height)
        .await
        .expect("a pinned lookup must not fail");
    assert_eq!(
        hash,
        Some(tip.hash),
        "the pinned view must still resolve its own tip height"
    );
    let height = clone
        .get_height_for_hash(tip.hash)
        .await
        .expect("a pinned lookup must not fail");
    assert_eq!(
        height,
        Some(tip.height),
        "the pinned view must still know its own tip hash"
    );

    let locator = BlockLocator::new(vec![tip]).expect("a single-entry locator is well-formed");
    let fork = clone
        .find_fork_point(&locator)
        .await
        .expect("fork-point detection must not fail");
    assert_eq!(
        fork,
        Some(tip),
        "the pinned view must still contain its own tip"
    );

    let after_stream: Vec<_> = clone
        .stream_raw_blocks(full_range.clone())
        .map(|item| item.expect("a streamed block must not be an error"))
        .collect()
        .await;
    assert_eq!(
        after_stream, before_stream,
        "the pinned view's blocks must survive the reorg byte for byte"
    );

    let after_block = clone
        .get_raw_block(tip.height)
        .await
        .expect("a pinned block read must not fail");
    assert_eq!(
        after_block, before_block,
        "a pinned block read must survive the reorg"
    );
    let after_header = clone
        .get_raw_block_header(tip.height)
        .await
        .expect("a pinned header read must not fail");
    assert_eq!(
        after_header, before_header,
        "a pinned header read must survive the reorg"
    );
    let after_treestate = clone
        .get_treestate(tip.height)
        .await
        .expect("a pinned treestate read must not fail");
    assert_eq!(
        after_treestate, before_treestate,
        "a pinned treestate must survive the reorg"
    );
    let after_mined = clone
        .get_mined_transaction(mined_txid)
        .await
        .expect("a pinned mined-transaction read must not fail");
    assert_eq!(
        after_mined, before_mined,
        "a pinned mined-transaction read must survive the reorg"
    );
    let after_status = clone
        .get_transaction_status(mined_txid)
        .await
        .expect("a pinned status read must not fail");
    assert_eq!(
        after_status, before_status,
        "a pinned transaction status must survive the reorg"
    );
    let after_unspent = clone
        .get_address_unspent_outpoints(address, full_range.clone())
        .await
        .expect("a pinned address query must not fail");
    assert_eq!(
        after_unspent, before_unspent,
        "a pinned unspent-outpoints answer must survive the reorg"
    );
    let after_txids = clone
        .get_address_transaction_ids(address, full_range)
        .await
        .expect("a pinned address-txids query must not fail");
    assert_eq!(
        after_txids, before_txids,
        "a pinned address-txids answer must survive the reorg"
    );
    let after_spend = clone
        .get_outpoint_spend_status(outpoint)
        .await
        .expect("a pinned spend-status read must not fail");
    assert_eq!(
        after_spend, before_spend,
        "a pinned spend status must survive the reorg"
    );
}

/// A mempool delivery is tagged with the tip it was validated
/// against.
///
/// The caller supplies `submit_transaction`, the
/// implementation-specific way to put one transaction into the
/// mempool, resolving to its txid.
///
/// Preconditions: the port is ready and the chain is quiescent while
/// the case runs.
pub async fn mempool_deliveries_are_tagged_with_the_current_tip<P>(
    port: &P,
    submit_transaction: impl Future<Output = TransactionHash>,
) where
    P: TakeSnapshot + SubscribeToMempool,
{
    let mut mempool = pin!(port.subscribe_to_mempool());
    let expected = submit_transaction.await;

    let delivered = next_delivery_of(
        mempool.as_mut(),
        expected,
        "the mempool stream must deliver a submitted transaction",
    )
    .await;

    let tip = ready_snapshot(port).await.get_pinned_tip();
    assert_eq!(
        delivered.validated_against, tip,
        "a mempool delivery must be tagged with the tip it was validated against"
    );
    assert!(
        !delivered.raw.as_slice().is_empty(),
        "a mempool delivery carries the transaction's serialization"
    );
}

/// A fresh mempool subscription delivers the current contents first: a
/// transaction the engine accepted before the subscription existed
/// arrives without any further submission. This is what makes
/// resubscription (ADR 0001's tip-event composition) sound — without
/// it, every resubscribe would silently drop the driver's own pending
/// transactions.
///
/// The caller supplies `transaction` — bytes the implementation
/// accepts as valid and does not already know.
///
/// Preconditions: the port is ready and the chain is quiescent while
/// the case runs.
pub async fn mempool_subscriptions_deliver_prior_contents<P>(port: &P, transaction: RawTransaction)
where
    P: BroadcastTransaction + SubscribeToMempool,
{
    let txid = port
        .broadcast_transaction(transaction.clone())
        .await
        .expect("a valid broadcast must be accepted");

    let mut mempool = pin!(port.subscribe_to_mempool());
    let delivered = next_delivery_of(
        mempool.as_mut(),
        txid,
        "a fresh subscription must deliver the current mempool contents first",
    )
    .await;
    assert_eq!(
        delivered.raw, transaction,
        "the prior contents must carry the transaction's bytes"
    );
}

/// ADR 0001's negative, executable: the mempool stream survives a tip
/// change — it does not end to signal one.
///
/// The caller supplies `move_chain` and `submit_transaction`, the
/// implementation-specific ways to move the chain once and to put one
/// transaction into the mempool afterwards. The stream, subscribed
/// before the movement, must still deliver the later submission,
/// tagged with the post-movement tip.
///
/// Preconditions: the port is ready and nothing else moves the chain
/// or feeds the mempool while the case runs.
pub async fn mempool_stream_survives_tip_changes<P>(
    port: &P,
    move_chain: impl Future<Output = ()>,
    submit_transaction: impl Future<Output = TransactionHash>,
) where
    P: TakeSnapshot + SubscribeToMempool,
{
    let mut mempool = pin!(port.subscribe_to_mempool());

    move_chain.await;
    let expected = submit_transaction.await;

    let delivered = next_delivery_of(
        mempool.as_mut(),
        expected,
        "the mempool stream must survive a tip change, not end on it",
    )
    .await;

    let tip = ready_snapshot(port).await.get_pinned_tip();
    assert_eq!(
        delivered.validated_against, tip,
        "a delivery after the tip change must be tagged with the new tip"
    );
}

/// An accepted broadcast is observable: the returned txid appears on
/// the mempool stream carrying the broadcast bytes.
///
/// The caller supplies `transaction` — bytes the implementation
/// accepts as valid (the mock takes any non-empty bytes; an engine
/// harness supplies a really signed transaction).
///
/// Preconditions: the port is ready, the transaction is valid for the
/// engine and not already known to it, and the chain is quiescent
/// while the case runs.
pub async fn broadcasts_reach_the_mempool<P>(port: &P, transaction: RawTransaction)
where
    P: TakeSnapshot + BroadcastTransaction + SubscribeToMempool,
{
    let mut mempool = pin!(port.subscribe_to_mempool());

    let txid = port
        .broadcast_transaction(transaction.clone())
        .await
        .expect("a valid broadcast must be accepted");

    let delivered = next_delivery_of(
        mempool.as_mut(),
        txid,
        "an accepted broadcast must appear on the mempool stream",
    )
    .await;
    assert_eq!(
        delivered.raw, transaction,
        "the mempool must deliver the broadcast transaction's bytes"
    );
}

/// Bytes that deserialize as no transaction are rejected as
/// malformed — a domain rejection, not a backend failure.
///
/// Precondition: the port is ready.
pub async fn malformed_broadcasts_are_rejected<P>(port: &P)
where
    P: BroadcastTransaction,
{
    let result = port
        .broadcast_transaction(RawTransaction::new(Vec::new()))
        .await;
    assert!(
        matches!(
            result,
            Err(PortError::Domain(BroadcastTransactionError::Malformed))
        ),
        "empty bytes must be rejected as malformed, got: {result:?}"
    );
}

/// An engine validation rejection is a domain answer carrying the
/// engine's reason — never a backend failure a driver would retry.
///
/// The caller supplies `rejectable` — well-formed bytes the engine
/// rejects in validation (the mock rejects bytes starting with
/// `reject:`; an engine harness supplies e.g. an expired or
/// double-spending transaction).
///
/// Precondition: the port is ready.
pub async fn rejected_broadcasts_are_domain_answers<P>(port: &P, rejectable: RawTransaction)
where
    P: BroadcastTransaction,
{
    let result = port.broadcast_transaction(rejectable).await;
    assert!(
        matches!(
            result,
            Err(PortError::Domain(BroadcastTransactionError::Rejected { .. }))
        ),
        "a validation rejection must be a domain answer carrying the engine's reason, got: {result:?}"
    );
}

/// The reported upgrade schedule is non-empty, ascends by activation
/// height, and each status agrees with the current tip: active means
/// reached, pending means not yet.
///
/// Preconditions: the port is ready and the chain is quiescent while
/// the case runs.
pub async fn reported_upgrades_agree_with_the_tip<P>(port: &P)
where
    P: TakeSnapshot + GetReportedUpgrades,
{
    let tip = ready_snapshot(port).await.get_pinned_tip();
    let upgrades = port
        .get_reported_upgrades()
        .await
        .expect("reporting the upgrade schedule must not fail");

    assert!(
        !upgrades.is_empty(),
        "the validator's schedule must report at least one upgrade — every Zcash chain has one in force"
    );

    let mut previous = None;
    for upgrade in &upgrades {
        if let Some(previous) = previous {
            assert!(
                upgrade.activation_height >= previous,
                "the schedule must ascend by activation height"
            );
        }
        previous = Some(upgrade.activation_height);

        match upgrade.status {
            UpgradeStatus::Active => assert!(
                upgrade.activation_height <= tip.height,
                "an active upgrade's activation height must be at or below the tip"
            ),
            UpgradeStatus::Pending => assert!(
                upgrade.activation_height > tip.height,
                "a pending upgrade's activation height must be above the tip"
            ),
        }
    }
}

/// A port that serves snapshots reports itself ready.
///
/// Precondition: the port is ready.
pub async fn ready_ports_report_ready<P>(port: &P)
where
    P: TakeSnapshot + GetHealth,
{
    ready_snapshot(port).await;
    let health = port
        .get_health()
        .await
        .expect("the health signal must not fail on a live port");
    assert_eq!(
        health,
        Health::Ready,
        "a port serving snapshots must report Ready"
    );
}

/// Shutdown ends the port: both subscription streams end, and every
/// subsequent operation — snapshots, broadcast, the upgrade schedule,
/// the health signal — fails with a fatal backend error, never a
/// domain answer from a dead port.
///
/// This case spends the port — pass a dedicated instance.
pub async fn shutdown_ends_the_port<P: DrivingPort>(port: &P) {
    let mut tips = pin!(port.subscribe_to_tip_changes());
    let mut mempool = pin!(port.subscribe_to_mempool());

    port.shut_down().await;

    expect_within(
        async { while tips.next().await.is_some() {} },
        "after shutdown, the tip stream must end",
    )
    .await;
    expect_within(
        async { while mempool.next().await.is_some() {} },
        "after shutdown, the mempool stream must end",
    )
    .await;

    assert_fatal_backend(port.take_snapshot().await.map(|_| ()), "taking a snapshot");
    assert_fatal_backend(
        port.broadcast_transaction(RawTransaction::new(b"post-shutdown".to_vec()))
            .await,
        "a broadcast",
    );
    assert_fatal_backend(port.get_reported_upgrades().await, "the upgrade schedule");
    assert_fatal_backend(port.get_health().await, "the health signal");
}

/// Genesis as the pinned view serves it.
///
/// Panics when genesis is absent — a conformance precondition for the
/// fork-point cases that anchor a locator at genesis.
async fn genesis_of<S: crate::snapshot::ChainSnapshot>(snapshot: &S) -> BlockId {
    let hash = snapshot
        .get_hash_for_height(Height::GENESIS)
        .await
        .expect("genesis lookup must not fail")
        .expect("conformance precondition: genesis must be in the pinned view");
    BlockId {
        height: Height::GENESIS,
        hash,
    }
}

/// Clones share the pinned view: a clone serves the same tip and the
/// same lookups as the snapshot it was cloned from.
///
/// Precondition: the port is ready.
pub async fn clones_share_the_pinned_view<P: TakeSnapshot>(port: &P) {
    let snapshot = ready_snapshot(port).await;
    let clone = snapshot.clone();

    assert_eq!(
        snapshot.get_pinned_tip(),
        clone.get_pinned_tip(),
        "a clone must be pinned to the same tip"
    );

    let genesis = Height::GENESIS;
    let original = snapshot
        .get_hash_for_height(genesis)
        .await
        .expect("genesis lookup must not fail");
    let cloned = clone
        .get_hash_for_height(genesis)
        .await
        .expect("genesis lookup must not fail");
    assert_eq!(original, cloned, "a clone must serve identical lookups");
}
