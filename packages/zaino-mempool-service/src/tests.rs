//! Test matrix for the mempool adapter layer, driven by in-crate mock ports.
//!
//! Split into two suites:
//! - [`core`] — the tip-agnostic [`MempoolService`]: set mirroring, incremental
//!   updates, capacity/DoS bounds, source-error degradation, the exclude filter,
//!   `getmempoolinfo`, the [`MempoolUpdate`] feed, and `source_tip` tagging.
//! - [`coherence`] — the tip-aware [`CoherenceService`]: freeze/thaw against the
//!   NS epoch, thaw-on-re-agreement, coherent reads, the raw-transaction stream,
//!   and validator-only mode.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zaino_primitives::types::{BlockHash, Height, TransactionId};
use zaino_source::{
    FailureMode, FetchError, GetMempoolMetadataError, GetMempoolSourceTipError,
    GetMempoolTxidsError, GetRawMempoolTransactionError, MempoolTxMeta, QueryError,
};

use zaino_mempool::config::MempoolConfig;
use zaino_mempool::ports::BlockRef;

#[cfg(feature = "tip_aware_mempool")]
use zaino_mempool::ports::{NfsEpochObserver, NonFinalizedEpoch};

// ---- mock ports --------------------------------------------------------

#[derive(Clone)]
struct MockTx {
    txid: TransactionId,
    entry_height: u32,
    entry_time: Option<i64>,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct MockSourceState {
    tip: Option<BlockRef>,
    mempool: Vec<MockTx>,
    /// Txids listed by metadata/txids but whose raw fetch returns `None`
    /// (the "left the mempool between listing and fetch" race).
    phantom: HashSet<TransactionId>,
    /// Txids whose raw fetch fails outright (an error, *not* "no such
    /// transaction").
    raw_fetch_error: HashSet<TransactionId>,
    raw_fetch_counts: HashMap<TransactionId, usize>,
    /// Number of verbose metadata listings served.
    metadata_fetch_count: usize,
    /// Number of `get_mempool_source_tip` reads served (C4: a poll must issue
    /// at most two — one opening the fetch window, one confirming tip stability).
    source_tip_reads: usize,
    /// Number of `get_mempool_txids` listings served — one per poll, so it is
    /// the poll counter the source-tip-read bound is measured against.
    txid_list_count: usize,
    /// If set, source calls fail with this message (a source outage).
    source_error: Option<String>,
}

#[derive(Clone)]
struct MockSource {
    state: Arc<Mutex<MockSourceState>>,
    /// Push-path wake, as the `zaino-state` sync loop supplies in production
    /// (§6). `fire_block_wake` models a block landing.
    block_wake: Arc<tokio::sync::watch::Sender<()>>,
}

impl MockSource {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockSourceState::default())),
            block_wake: Arc::new(tokio::sync::watch::channel(()).0),
        }
    }

    fn fire_block_wake(&self) {
        let _ = self.block_wake.send(());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockSourceState> {
        self.state.lock().expect("mock source poisoned")
    }

    fn set_mempool(&self, mempool: Vec<MockTx>) {
        self.lock().mempool = mempool;
    }

    fn set_tip(&self, tip: BlockRef) {
        self.lock().tip = Some(tip);
    }

    fn set_error(&self, message: Option<&str>) {
        self.lock().source_error = message.map(str::to_string);
    }

    fn fail_raw_fetch_for(&self, txid: TransactionId) {
        self.lock().raw_fetch_error.insert(txid);
    }

    fn raw_fetch_count(&self, txid: &TransactionId) -> usize {
        self.lock().raw_fetch_counts.get(txid).copied().unwrap_or(0)
    }

    fn metadata_fetch_count(&self) -> usize {
        self.lock().metadata_fetch_count
    }

    fn source_tip_reads(&self) -> usize {
        self.lock().source_tip_reads
    }

    fn txid_list_count(&self) -> usize {
        self.lock().txid_list_count
    }

    fn error_message(&self) -> Option<String> {
        self.lock().source_error.clone()
    }
}

/// The source outage the mock injects, as each port's own error type.
///
/// A transport failure rather than a domain rejection: every port's domain error
/// means the validator answered, and the mempool must degrade the same way for
/// either — so a `Fetch` failure is the honest shape for "the source is down".
fn outage<E: std::fmt::Debug + std::fmt::Display>(message: String) -> QueryError<E> {
    QueryError::Fetch(FetchError::new(FailureMode::Connection, message))
}

impl zaino_source::GetMempoolTxids for MockSource {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError>> {
        if let Some(message) = self.error_message() {
            return Err(outage(message));
        }
        let mut state = self.lock();
        state.txid_list_count += 1;
        Ok(state.mempool.iter().map(|tx| tx.txid).collect())
    }
}

impl zaino_source::GetMempoolMetadata for MockSource {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError>> {
        if let Some(message) = self.error_message() {
            return Err(outage(message));
        }
        self.lock().metadata_fetch_count += 1;
        Ok(self
            .lock()
            .mempool
            .iter()
            .map(|tx| MempoolTxMeta {
                txid: tx.txid,
                entry_height: height(tx.entry_height),
                entry_time: tx.entry_time,
            })
            .collect())
    }
}

impl zaino_source::GetRawMempoolTransaction for MockSource {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        let mut state = self.lock();
        *state.raw_fetch_counts.entry(txid).or_default() += 1;
        if state.raw_fetch_error.contains(&txid) {
            return Err(outage("unmodelled validator error".to_string()));
        }
        // The modelled "it left the mempool between listing and fetch" answer,
        // which the service must treat differently from the outage above.
        if state.phantom.contains(&txid) {
            return Err(QueryError::Domain(GetRawMempoolTransactionError::NotFound(
                txid,
            )));
        }
        state
            .mempool
            .iter()
            .find(|tx| tx.txid == txid)
            .map(|tx| tx.bytes.clone())
            .ok_or(QueryError::Domain(GetRawMempoolTransactionError::NotFound(
                txid,
            )))
    }
}

impl zaino_source::GetMempoolSourceTip for MockSource {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<(BlockHash, Height), QueryError<GetMempoolSourceTipError>> {
        self.lock().source_tip_reads += 1;
        if let Some(message) = self.error_message() {
            return Err(outage(message));
        }
        match self.lock().tip {
            Some(tip) => Ok((tip.hash, tip.height)),
            None => Err(QueryError::Domain(GetMempoolSourceTipError::NotReady)),
        }
    }
}

impl zaino_source::SubscribeBlocks for MockSource {
    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.block_wake.subscribe())
    }
}

#[cfg(feature = "tip_aware_mempool")]
#[derive(Clone)]
struct MockNfs {
    epoch: Arc<Mutex<Option<NonFinalizedEpoch>>>,
    /// Publication signal, as the real `zaino-state` adapter supplies.
    wake: Arc<tokio::sync::watch::Sender<()>>,
}

#[cfg(feature = "tip_aware_mempool")]
impl MockNfs {
    fn new() -> Self {
        Self {
            epoch: Arc::new(Mutex::new(None)),
            wake: Arc::new(tokio::sync::watch::channel(()).0),
        }
    }

    fn set(&self, epoch: NonFinalizedEpoch) {
        *self.epoch.lock().expect("mock nfs poisoned") = Some(epoch);
        let _ = self.wake.send(());
    }
}

#[cfg(feature = "tip_aware_mempool")]
impl NfsEpochObserver for MockNfs {
    fn current_epoch(&self) -> Option<NonFinalizedEpoch> {
        *self.epoch.lock().expect("mock nfs poisoned")
    }

    fn subscribe_epoch_changes(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.wake.subscribe())
    }
}

// ---- helpers -----------------------------------------------------------

fn height(height: u32) -> Height {
    Height::try_from(height).expect("valid fixture height")
}

fn txid(byte: u8) -> TransactionId {
    TransactionId::from([byte; 32])
}

/// A distinct txid derived from an index (for large-mempool tests).
fn txid_n(n: u32) -> TransactionId {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    TransactionId::from(bytes)
}

fn mtx(byte: u8, entry_height: u32) -> MockTx {
    MockTx {
        txid: txid(byte),
        entry_height,
        entry_time: None,
        bytes: vec![byte],
    }
}

fn block_ref(at: u32, hash_byte: u8) -> BlockRef {
    BlockRef {
        hash: BlockHash::from([hash_byte; 32]),
        height: height(at),
    }
}

#[cfg(feature = "tip_aware_mempool")]
fn epoch(generation: u64, height: u32, hash_byte: u8) -> NonFinalizedEpoch {
    NonFinalizedEpoch {
        generation,
        best_tip: block_ref(height, hash_byte),
    }
}

fn fast_config() -> MempoolConfig {
    let mut config = MempoolConfig::default();
    config.poll_interval = Duration::from_millis(5);
    // Keep the metadata floor at the poll cadence, as the default does — a test
    // that wants coalescing raises it explicitly.
    config.metadata_min_interval = config.poll_interval;
    config
}

// ============================ CORE ============================

mod core {
    use super::*;

    use crate::{MempoolService, MempoolSubscriber};
    use futures::StreamExt as _;
    use zaino_mempool::snapshot::{MempoolCompleteness, MempoolSnapshot};
    use zaino_mempool::update::MempoolUpdate;

    fn spawn_core(
        height: u32,
        hash_byte: u8,
        mempool: Vec<MockTx>,
        config: MempoolConfig,
    ) -> (
        Arc<MempoolService<MockSource>>,
        MempoolSubscriber,
        MockSource,
    ) {
        let source = MockSource::new();
        source.set_tip(block_ref(height, hash_byte));
        source.set_mempool(mempool);
        let service = MempoolService::spawn(source.clone(), config, CancellationToken::new());
        let subscriber = service.subscriber();
        (service, subscriber, source)
    }

    /// Polls until `predicate` holds, or panics after a 10s budget.
    ///
    /// The budget is a starvation allowance, not a timing assertion: the service
    /// converges in milliseconds, and the only thing that makes it take longer is
    /// a CI machine running the rest of the suite alongside it. 5s was not enough
    /// for that.
    async fn wait_for(
        subscriber: &MempoolSubscriber,
        predicate: impl Fn(&MempoolSnapshot) -> bool,
    ) -> Arc<MempoolSnapshot> {
        for _ in 0..2000 {
            let snapshot = subscriber.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("core snapshot never satisfied the predicate");
    }

    #[tokio::test]
    async fn mirrors_source_mempool_and_tags_the_tip() {
        let (service, subscriber, _source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], fast_config());

        let snapshot = wait_for(&subscriber, |s| s.tx_count == 2).await;
        assert_eq!(snapshot.completeness, MempoolCompleteness::Complete);
        assert_eq!(snapshot.source_tip, Some(block_ref(100, 0xAB)));
        assert_eq!(snapshot.by_txid[&txid(1)].entry_height, height(100));

        service.close();
    }

    #[tokio::test]
    async fn does_not_freeze_on_validator_tip_change() {
        // The core is tip-agnostic: a validator-tip advance re-tags the set but
        // never freezes — the live view stays served.
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count == 1).await;

        source.set_tip(block_ref(101, 0xCD));

        let snapshot = wait_for(&subscriber, |s| s.source_tip == Some(block_ref(101, 0xCD))).await;
        assert_eq!(snapshot.completeness, MempoolCompleteness::Complete);
        assert_eq!(snapshot.tx_count, 1);
        service.close();
    }

    #[tokio::test]
    async fn source_tip_reads_are_bounded_to_two_per_poll() {
        // C4: each poll opens its fetch window with one tip read and confirms
        // tip stability with a second — never a third. The old pre-fetch read is
        // gone. Measured as an invariant over a window of real polls, including
        // ones that change the set (which take the full two-read path).
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count == 1).await;

        // Exercise the two-read path: a tip change and a set change each force a
        // publish, whose tip-stability guard issues the second read.
        source.set_tip(block_ref(101, 0xCD));
        wait_for(&subscriber, |s| s.source_tip == Some(block_ref(101, 0xCD))).await;
        source.set_mempool(vec![mtx(1, 100), mtx(2, 101)]);
        wait_for(&subscriber, |s| s.tx_count == 2).await;

        // Let several steady polls (one read each) accumulate too.
        tokio::time::sleep(Duration::from_millis(60)).await;

        let reads = source.source_tip_reads();
        let polls = source.txid_list_count();
        assert!(
            polls >= 3,
            "expected several polls to have elapsed, got {polls}"
        );
        // At most two reads per poll. The `+ 1` tolerates sampling mid-poll,
        // after a poll's opening read but before that poll lists its txids.
        assert!(
            reads <= 2 * polls + 1,
            "source-tip reads {reads} exceeded two per poll over {polls} polls"
        );

        service.close();
    }

    #[tokio::test]
    async fn block_burst_wake_does_not_trip_the_discard_backstop() {
        // §6: the sync-loop push path can fire block-wakes in a burst (rapid
        // mining, catch-up). watch coalescing plus a fresh tip read per tick means
        // the burst collapses to a few stable ticks — it must not drive the
        // tag-stability guard to MAX_CONSECUTIVE_DISCARDS and republish the set as
        // IncompleteSourceError.
        //
        // The poll interval is far longer than the burst, so convergence is
        // driven by the wakes rather than the timer — but not so long that the
        // test hangs when a wake is consumed by a poll that does not converge.
        // That is a real interleaving: the core can be mid-`tick` when the burst
        // lands, so the coalesced wake is spent on a poll that reads a mid-burst
        // tip and is correctly discarded. At the 30s interval this test used to
        // carry, the next tick was then 30 seconds away and the test failed on a
        // service that had done nothing wrong — about one run in ten. Production
        // polls at 500ms, so the recovery this bounds is sub-second there.
        let mut config = fast_config();
        config.poll_interval = Duration::from_secs(1);
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], config);

        // The immediate first interval tick serves the initial set.
        wait_for(&subscriber, |s| s.tx_count == 1).await;

        // A burst: advance the tip many times and fire a wake each time, all with
        // no `.await` between them, so the core task stays parked until the tip
        // has settled. The wakes coalesce; the core reads only the final tip.
        for i in 1..=20u8 {
            source.set_tip(block_ref(100 + u32::from(i), 0xB0 + i));
            source.fire_block_wake();
        }
        // Settle on a final, stable tip and a changed set.
        source.set_tip(block_ref(200, 0xFF));
        source.set_mempool(vec![mtx(1, 100), mtx(2, 200)]);
        source.fire_block_wake();

        // Converges to the final tip as a Complete set — the backstop never
        // latched IncompleteSourceError.
        let snapshot = wait_for(&subscriber, |s| {
            s.source_tip == Some(block_ref(200, 0xFF)) && s.tx_count == 2
        })
        .await;

        assert_eq!(snapshot.completeness, MempoolCompleteness::Complete);

        // The claim the burst is here to test, and which the timing cliff above
        // was standing in for: 21 wakes did not become 21 polls. `watch`
        // coalesces them, so the core reads the settled tip a handful of times
        // instead of chasing every intermediate one — which is what keeps the
        // tag-stability guard away from its backstop in the first place.
        let polls = source.txid_list_count();
        assert!(
            polls < 10,
            "a burst of 21 wakes produced {polls} polls; watch coalescing is not working"
        );

        service.close();
    }

    #[tokio::test]
    async fn added_transaction_is_fetched_once() {
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count == 2).await;

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(source.raw_fetch_count(&txid(1)), 1);
        assert_eq!(source.raw_fetch_count(&txid(2)), 1);
        service.close();
    }

    #[tokio::test]
    async fn removed_transaction_is_dropped() {
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count == 2).await;

        source.set_mempool(vec![mtx(1, 100)]);

        let snapshot = wait_for(&subscriber, |s| s.tx_count == 1).await;
        assert!(snapshot.by_txid.contains_key(&txid(1)));
        assert!(!snapshot.by_txid.contains_key(&txid(2)));
        service.close();
    }

    #[tokio::test]
    async fn transaction_that_disappears_before_fetch_is_skipped() {
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], fast_config());
        source.lock().phantom.insert(txid(2));

        let snapshot = wait_for(&subscriber, |s| s.tx_count == 1).await;
        assert!(snapshot.by_txid.contains_key(&txid(1)));
        assert!(!snapshot.by_txid.contains_key(&txid(2)));
        service.close();
    }

    #[tokio::test]
    async fn entry_time_is_propagated() {
        let source = MockSource::new();
        source.set_tip(block_ref(100, 0xAB));
        source.set_mempool(vec![MockTx {
            txid: txid(1),
            entry_height: 100,
            entry_time: Some(1_700_000_000),
            bytes: vec![1],
        }]);
        let service =
            MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let subscriber = service.subscriber();

        let snapshot = wait_for(&subscriber, |s| s.tx_count == 1).await;
        assert_eq!(snapshot.by_txid[&txid(1)].entry_time, Some(1_700_000_000));
        service.close();
    }

    #[tokio::test]
    async fn source_error_keeps_set_and_marks_incomplete() {
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count == 1).await;

        source.set_error(Some("validator unreachable"));

        let snapshot = wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::IncompleteSourceError
        })
        .await;
        assert_eq!(snapshot.tx_count, 1); // prior set preserved, never frozen away
        service.close();
    }

    #[tokio::test]
    async fn raw_fetch_error_degrades_completeness_and_keeps_the_set() {
        // A raw fetch that *errors* is not a "no such transaction": the poll's
        // update is abandoned, the held set survives, and the set is reported
        // incomplete. Nothing is deleted on the strength of an unmodelled
        // failure.
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count == 1).await;

        source.fail_raw_fetch_for(txid(2));
        source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);

        let snapshot = wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::IncompleteSourceError
        })
        .await;
        assert!(snapshot.by_txid.contains_key(&txid(1)));
        assert!(!snapshot.by_txid.contains_key(&txid(2)));
        service.close();
    }

    #[tokio::test]
    async fn capacity_bound_drops_additions_and_marks_incomplete() {
        // A memory bound below the cost of the additions: the core must not exceed
        // it, so it drops the additions and marks the set capacity-limited.
        let config = fast_config();
        // Each tx costs the ZIP-401 floor (10_000). Bound at one tx worth.
        config.set_max_cost_bytes(zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD);
        let (service, subscriber, _source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], config);

        let snapshot = wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;
        // Partial admission: the set fills up to the bound rather than dropping
        // every addition.
        assert_eq!(snapshot.tx_count, 1);
        service.close();
    }

    #[tokio::test]
    async fn capacity_bound_limits_fetches_not_just_retention() {
        // The bound must cap the *fetch*, not only the retained set: fetching
        // everything and refusing afterwards performs the whole memory blow-up
        // the bound exists to prevent. With room for one transaction, exactly one
        // raw fetch may be issued — the other is refused sight unseen and
        // remembered, so it is not rediscovered and re-fetched every poll either.
        let config = fast_config();
        config.set_max_cost_bytes(zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD);
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], config);

        wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;

        // Many poll intervals pass without the refusal being retried.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let fetches = source.raw_fetch_count(&txid(1)) + source.raw_fetch_count(&txid(2));
        assert_eq!(
            fetches, 1,
            "only the admissible transaction may be fetched; the refused one \
             must never be pulled into memory"
        );

        let snapshot = subscriber.snapshot();
        assert_eq!(snapshot.tx_count, 1);
        // The shortfall is named, so a caller can tell "short this one" from
        // "no such transaction".
        assert_eq!(snapshot.unadmitted.len(), 1);
        service.close();
    }

    #[tokio::test]
    async fn set_at_bound_skips_the_metadata_walk() {
        // At the bound there is nothing to admit, so the whole-mempool metadata
        // listing must not be issued at all — it is the dominant per-poll cost
        // and would buy nothing.
        let config = fast_config();
        config.set_max_cost_bytes(zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD);
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], config);

        wait_for(&subscriber, |s| s.tx_count == 1).await;
        let listings_once_full = source.metadata_fetch_count();

        // A new arrival cannot fit: headroom is zero.
        source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
        wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;
        tokio::time::sleep(Duration::from_millis(60)).await;

        assert_eq!(
            source.metadata_fetch_count(),
            listings_once_full,
            "metadata listing issued despite there being no headroom to admit into"
        );
        assert_eq!(source.raw_fetch_count(&txid(2)), 0);
        service.close();
    }

    #[tokio::test]
    async fn low_water_retry_works_for_small_bounds() {
        // The low-water mark is `max * pct / 100`. Computed as `max / 100 * pct`
        // it truncates to zero for any bound below 100, `cost_bytes >= 0` always
        // holds, and refusals are stranded forever. Operator-reachable via
        // `[mempool] max_cost_bytes`.
        let config = fast_config();
        config.set_max_cost_bytes(50);
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], config);

        // Nothing fits in 50 bytes (the ZIP-401 floor alone is 10,000), so the
        // transaction is refused and remembered.
        wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;

        // It leaves the mempool: the memo must let go of it rather than holding
        // a permanently un-retryable entry.
        source.set_mempool(Vec::new());
        let snapshot = wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::Complete
        })
        .await;
        assert!(snapshot.unadmitted.is_empty());
        service.close();
    }

    #[tokio::test]
    async fn admission_tiebreak_depends_on_the_salt() {
        // Admission order must not be predictable from the txid alone, or a
        // sender can grind a low-sorting txid to displace honest transactions at
        // capacity. Asserted as the deterministic property — same salt, same
        // order; different salt, different order — rather than by sampling.
        // Same salt twice: identical admission.
        let first = admitted_txid_with_salt(7).await;
        assert_eq!(
            first,
            admitted_txid_with_salt(7).await,
            "the same salt must give the same admission order"
        );

        // Some other salt admits the other transaction: the order is a function
        // of the salt, not of the txid bytes. Each salt is a fixed computation,
        // so this is deterministic — it just walks a short list until one flips.
        let mut flipped = false;
        for salt in (0..32u64).map(|n| n.wrapping_mul(2_654_435_761)) {
            if admitted_txid_with_salt(salt).await != first {
                flipped = true;
                break;
            }
        }
        assert!(
            flipped,
            "no salt changed the admission order — the tiebreak is not salted"
        );
    }

    /// Spawn a core with room for exactly one transaction and two competing
    /// additions, and report which one was admitted under `salt`.
    async fn admitted_txid_with_salt(salt: u64) -> TransactionId {
        let config = fast_config();
        config.set_max_cost_bytes(zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD);
        let source = MockSource::new();
        source.set_tip(block_ref(100, 0xAB));
        // Same entry_time and height, so only the tiebreak separates them.
        source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
        let service = MempoolService::spawn_with_admission_salt(
            source,
            config,
            CancellationToken::new(),
            salt,
        );
        let subscriber = service.subscriber();
        let snapshot = wait_for(&subscriber, |s| s.tx_count == 1).await;
        let admitted = *snapshot
            .by_txid
            .keys()
            .next()
            .expect("exactly one transaction admitted");
        service.close();
        admitted
    }

    #[tokio::test]
    async fn refused_transactions_are_admitted_once_there_is_headroom() {
        // Raising the bound clears the refusal memo (the set is now well below
        // the low-water mark), so the refused transaction is admitted.
        let config = fast_config();
        config.set_max_cost_bytes(zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD);
        let (service, subscriber, _source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], config);

        wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;

        service.set_max_cost_bytes(1_000_000);

        let snapshot = wait_for(&subscriber, |s| s.tx_count == 2).await;
        assert_eq!(snapshot.completeness, MempoolCompleteness::Complete);
        service.close();
    }

    #[tokio::test]
    async fn metadata_listing_is_floored_by_min_interval() {
        // The verbose listing is heavy on the source, so it is rate-floored. A
        // poll inside the floor defers the *additions* — and says so, rather than
        // presenting a short set as complete.
        let mut config = fast_config();
        config.metadata_min_interval = Duration::from_secs(30);
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], config);

        wait_for(&subscriber, |s| s.tx_count == 1).await;
        let listings_after_startup = source.metadata_fetch_count();

        source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            source.metadata_fetch_count(),
            listings_after_startup,
            "listing re-issued inside the floor"
        );
        let snapshot = subscriber.snapshot();
        assert_eq!(snapshot.tx_count, 1);
        assert_eq!(
            snapshot.completeness,
            MempoolCompleteness::IncompletePendingMetadata,
            "a deferred addition must be reported as such, not as a complete set"
        );
        assert!(snapshot.unadmitted.contains(&txid(2)));
        service.close();
    }

    #[tokio::test]
    async fn metadata_deferral_still_publishes_removals_and_retag() {
        // Deferring additions must not withhold the rest of the poll: coherence
        // thaws on the tip re-tag, so holding it back would make
        // `metadata_min_interval` extend the post-block freeze by its own length.
        let mut config = fast_config();
        config.metadata_min_interval = Duration::from_secs(30);
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], config);

        wait_for(&subscriber, |s| s.tx_count == 2).await;

        // One transaction leaves, another arrives, and the tip advances — all in
        // the same poll, with the metadata listing floored out.
        source.set_mempool(vec![mtx(2, 100), mtx(3, 100)]);
        source.set_tip(block_ref(101, 0xCD));

        let snapshot = wait_for(&subscriber, |s| s.source_tip == Some(block_ref(101, 0xCD))).await;
        assert!(
            !snapshot.by_txid.contains_key(&txid(1)),
            "the removal must apply even though the additions were deferred"
        );
        assert!(
            !snapshot.by_txid.contains_key(&txid(3)),
            "the addition must still be deferred"
        );
        assert!(snapshot.unadmitted.contains(&txid(3)));
        service.close();
    }

    #[tokio::test]
    async fn get_mempool_info_reports_totals() {
        let (service, subscriber, _source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count == 2).await;

        let info = subscriber.get_mempool_info();
        assert_eq!(info.size, 2);
        assert_eq!(info.bytes, 2); // two 1-byte txs
        service.close();
    }

    #[tokio::test]
    async fn exclude_filter_semantics() {
        // Two txs whose txids differ only in the leading byte; a suffix matching
        // both excludes neither, a suffix matching one excludes it.
        let mut a = [0u8; 32];
        a[0] = 0x01;
        let mut b = [0u8; 32];
        b[0] = 0x02;
        let source = MockSource::new();
        source.set_tip(block_ref(100, 0xAB));
        source.set_mempool(vec![
            MockTx {
                txid: TransactionId::from(a),
                entry_height: 100,
                entry_time: None,
                bytes: vec![1],
            },
            MockTx {
                txid: TransactionId::from(b),
                entry_height: 100,
                entry_time: None,
                bytes: vec![2],
            },
        ]);
        let service =
            MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let subscriber = service.subscriber();
        wait_for(&subscriber, |s| s.tx_count == 2).await;

        // Suffix `[0x00; 31]` (the shared trailing bytes) matches both -> excludes
        // neither.
        let shared = subscriber
            .validate_exclude_suffixes(&[vec![0u8; 31]])
            .expect("valid");
        assert_eq!(subscriber.get_filtered_entries(&shared).len(), 2);

        // A suffix that uniquely identifies `a` excludes just it.
        let mut unique = vec![0u8; 32];
        unique[0] = 0x01;
        let unique = subscriber
            .validate_exclude_suffixes(&[unique])
            .expect("valid");
        let kept = subscriber.get_filtered_entries(&unique);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].txid, TransactionId::from(b));
        service.close();
    }

    #[tokio::test]
    async fn update_feed_emits_added_removed_and_reset() {
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count == 1).await;

        let mut updates = subscriber.subscribe_updates();
        source.set_mempool(vec![mtx(2, 100)]); // remove 1, add 2

        let mut saw_added = false;
        let mut saw_removed = false;
        let mut saw_reset = false;
        for _ in 0..100 {
            match tokio::time::timeout(Duration::from_millis(200), updates.recv()).await {
                Ok(Ok(MempoolUpdate::Added { entry, .. })) if entry.txid == txid(2) => {
                    saw_added = true;
                }
                Ok(Ok(MempoolUpdate::Removed { txid: t, .. })) if t == txid(1) => {
                    saw_removed = true;
                }
                Ok(Ok(MempoolUpdate::Reset { .. })) => saw_reset = true,
                Ok(Ok(_)) => {}
                _ => break,
            }
            if saw_added && saw_removed && saw_reset {
                break;
            }
        }
        assert!(saw_added && saw_removed && saw_reset);
        service.close();
    }

    #[tokio::test]
    async fn mempool_updates_stream_yields_changes() {
        // The ergonomic `mempool_updates()` Stream form delivers the same deltas.
        let source = MockSource::new();
        source.set_tip(block_ref(100, 0xAB));
        let service =
            MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let subscriber = service.subscriber();
        wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::Complete
        })
        .await;

        // Subscribe (via the stream) before mutating, per the consistency contract.
        let mut updates = Box::pin(subscriber.mempool_updates());
        source.set_mempool(vec![mtx(1, 100)]);

        let mut saw_added = false;
        let mut saw_reset = false;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_millis(200), updates.next()).await {
                Ok(Some(MempoolUpdate::Added { entry, .. })) if entry.txid == txid(1) => {
                    saw_added = true;
                }
                Ok(Some(MempoolUpdate::Reset { .. })) => saw_reset = true,
                Ok(Some(_)) => {}
                _ => break,
            }
            if saw_added && saw_reset {
                break;
            }
        }
        assert!(saw_added && saw_reset);
        service.close();
    }

    #[tokio::test]
    async fn mempool_updates_reports_lag_explicitly() {
        // A consumer that falls behind the bounded feed is told so *in band*
        // (never a silent skip), so it can resync from `current()`.
        let mut config = fast_config();
        config.event_buffer_len = 2; // tiny buffer: one publish overflows it

        let source = MockSource::new();
        source.set_tip(block_ref(100, 0xAB));
        let service = MempoolService::spawn(source.clone(), config, CancellationToken::new());
        let subscriber = service.subscriber();
        wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::Complete
        })
        .await;

        // Subscribe, then flood far more updates than the 2-slot buffer while the
        // stream is not polled.
        let mut updates = Box::pin(subscriber.mempool_updates());
        let flood: Vec<MockTx> = (0..50)
            .map(|n| MockTx {
                txid: txid_n(n),
                entry_height: 100,
                entry_time: None,
                bytes: vec![(n % 251) as u8],
            })
            .collect();
        source.set_mempool(flood);
        tokio::time::sleep(Duration::from_millis(80)).await;

        // The first delivered item is an explicit Lagged, not a dropped delta.
        let first = tokio::time::timeout(Duration::from_millis(200), updates.next())
            .await
            .expect("stream produced an item")
            .expect("stream not ended");
        assert!(
            matches!(first, MempoolUpdate::Lagged { .. }),
            "expected explicit lag signal, got {first:?}"
        );
        service.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_readers_see_a_growing_set() {
        // multi_thread required: many reader tasks run concurrently with the
        // writer task, exercising the lock-free ArcSwap serving path under load.
        let source = MockSource::new();
        source.set_tip(block_ref(100, 0xAB));
        let mempool: Vec<MockTx> = (0..200)
            .map(|n| MockTx {
                txid: txid_n(n),
                entry_height: 100,
                entry_time: None,
                bytes: vec![(n % 251) as u8],
            })
            .collect();
        source.set_mempool(mempool);
        let service =
            MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let subscriber = service.subscriber();
        wait_for(&subscriber, |s| s.tx_count == 200).await;

        let mut handles = Vec::new();
        for _ in 0..16 {
            let sub = subscriber.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..500 {
                    let snap = sub.snapshot();
                    assert_eq!(snap.tx_count, snap.by_txid.len());
                }
            }));
        }
        for handle in handles {
            handle.await.expect("reader task panicked");
        }
        service.close();
    }
}

// ============================ COHERENCE ============================
