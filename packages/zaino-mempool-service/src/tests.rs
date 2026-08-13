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
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zaino_primitives::types::{BlockHash, Height, TransactionId};
use zaino_source::{
    FailureMode, FetchError, GetMempoolMetadataError, GetMempoolTxidsError,
    GetRawMempoolTransactionError, MempoolTxMeta, QueryError,
};

use zaino_mempool::config::MempoolConfig;
use zaino_primitives::types::BlockRef;

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
    ) -> Result<(BlockHash, Height), QueryError<std::convert::Infallible>> {
        self.lock().source_tip_reads += 1;
        if let Some(message) = self.error_message() {
            return Err(outage(message));
        }
        match self.lock().tip {
            Some(tip) => Ok((tip.hash, tip.height)),
            // No domain answer on this port by design — an unset fixture tip is
            // a fault, not the validator reporting something.
            None => Err(outage("mock source has no tip set".to_string())),
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
    config.set_poll_interval_ms(NonZeroU64::new(5).expect("non-zero test interval"));
    // Keep the metadata floor at the poll cadence, as the default does — a test
    // that wants coalescing raises it explicitly.
    config.set_metadata_min_interval(config.poll_interval());
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

        let snapshot = wait_for(&subscriber, |s| s.tx_count() == 2).await;
        assert_eq!(snapshot.completeness(), MempoolCompleteness::Complete);
        assert_eq!(snapshot.source_tip(), Some(block_ref(100, 0xAB)));
        assert_eq!(snapshot.by_txid()[&txid(1)].entry_height, height(100));

        service.close();
    }

    #[tokio::test]
    async fn does_not_freeze_on_validator_tip_change() {
        // The core is tip-agnostic: a validator-tip advance re-tags the set but
        // never freezes — the live view stays served.
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count() == 1).await;

        source.set_tip(block_ref(101, 0xCD));

        let snapshot = wait_for(&subscriber, |s| {
            s.source_tip() == Some(block_ref(101, 0xCD))
        })
        .await;
        assert_eq!(snapshot.completeness(), MempoolCompleteness::Complete);
        assert_eq!(snapshot.tx_count(), 1);
        service.close();
    }

    #[tokio::test]
    async fn source_tip_reads_are_bounded_to_two_per_poll() {
        // C4: each poll opens its fetch window with one tip read and confirms
        // tip stability with a second — never a third. The old pre-fetch read is
        // gone. Measured as an invariant over a window of real polls, including
        // ones that change the set (which take the full two-read path).
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count() == 1).await;

        // Exercise the two-read path: a tip change and a set change each force a
        // publish, whose tip-stability guard issues the second read.
        source.set_tip(block_ref(101, 0xCD));
        wait_for(&subscriber, |s| {
            s.source_tip() == Some(block_ref(101, 0xCD))
        })
        .await;
        source.set_mempool(vec![mtx(1, 100), mtx(2, 101)]);
        wait_for(&subscriber, |s| s.tx_count() == 2).await;

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
        config.set_poll_interval_ms(NonZeroU64::new(1_000).expect("non-zero test interval"));
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], config);

        // The immediate first interval tick serves the initial set.
        wait_for(&subscriber, |s| s.tx_count() == 1).await;

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
            s.source_tip() == Some(block_ref(200, 0xFF)) && s.tx_count() == 2
        })
        .await;

        assert_eq!(snapshot.completeness(), MempoolCompleteness::Complete);

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
        wait_for(&subscriber, |s| s.tx_count() == 2).await;

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(source.raw_fetch_count(&txid(1)), 1);
        assert_eq!(source.raw_fetch_count(&txid(2)), 1);
        service.close();
    }

    #[tokio::test]
    async fn removed_transaction_is_dropped() {
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count() == 2).await;

        source.set_mempool(vec![mtx(1, 100)]);

        let snapshot = wait_for(&subscriber, |s| s.tx_count() == 1).await;
        assert!(snapshot.by_txid().contains_key(&txid(1)));
        assert!(!snapshot.by_txid().contains_key(&txid(2)));
        service.close();
    }

    #[tokio::test]
    async fn transaction_that_disappears_before_fetch_is_skipped() {
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], fast_config());
        source.lock().phantom.insert(txid(2));

        let snapshot = wait_for(&subscriber, |s| s.tx_count() == 1).await;
        assert!(snapshot.by_txid().contains_key(&txid(1)));
        assert!(!snapshot.by_txid().contains_key(&txid(2)));
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

        let snapshot = wait_for(&subscriber, |s| s.tx_count() == 1).await;
        assert_eq!(snapshot.by_txid()[&txid(1)].entry_time, Some(1_700_000_000));
        service.close();
    }

    #[tokio::test]
    async fn source_error_keeps_set_and_marks_incomplete() {
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count() == 1).await;

        source.set_error(Some("validator unreachable"));

        let snapshot = wait_for(&subscriber, |s| {
            s.completeness() == MempoolCompleteness::IncompleteSourceError
        })
        .await;
        assert_eq!(snapshot.tx_count(), 1); // prior set preserved, never frozen away
        service.close();
    }

    #[tokio::test]
    async fn raw_fetch_error_degrades_completeness_and_keeps_the_set() {
        // A raw fetch that *errors* is not a "no such transaction": the poll's
        // update is abandoned, the held set survives, and the set is reported
        // incomplete. Nothing is deleted on the strength of an unmodelled
        // failure.
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count() == 1).await;

        source.fail_raw_fetch_for(txid(2));
        source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);

        let snapshot = wait_for(&subscriber, |s| {
            s.completeness() == MempoolCompleteness::IncompleteSourceError
        })
        .await;
        assert!(snapshot.by_txid().contains_key(&txid(1)));
        assert!(!snapshot.by_txid().contains_key(&txid(2)));
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
            s.completeness() == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;
        // Partial admission: the set fills up to the bound rather than dropping
        // every addition.
        assert_eq!(snapshot.tx_count(), 1);
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
            s.completeness() == MempoolCompleteness::IncompleteCapacityLimited
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
        assert_eq!(snapshot.tx_count(), 1);
        // The shortfall is named, so a caller can tell "short this one" from
        // "no such transaction".
        assert_eq!(snapshot.unadmitted().len(), 1);
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

        wait_for(&subscriber, |s| s.tx_count() == 1).await;
        let listings_once_full = source.metadata_fetch_count();

        // A new arrival cannot fit: headroom is zero.
        source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
        wait_for(&subscriber, |s| {
            s.completeness() == MempoolCompleteness::IncompleteCapacityLimited
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
            s.completeness() == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;

        // It leaves the mempool: the memo must let go of it rather than holding
        // a permanently un-retryable entry.
        source.set_mempool(Vec::new());
        let snapshot = wait_for(&subscriber, |s| {
            s.completeness() == MempoolCompleteness::Complete
        })
        .await;
        assert!(snapshot.unadmitted().is_empty());
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
        let snapshot = wait_for(&subscriber, |s| s.tx_count() == 1).await;
        let admitted = *snapshot
            .by_txid()
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
            s.completeness() == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;

        service.set_max_cost_bytes(1_000_000);

        let snapshot = wait_for(&subscriber, |s| s.tx_count() == 2).await;
        assert_eq!(snapshot.completeness(), MempoolCompleteness::Complete);
        service.close();
    }

    #[tokio::test]
    async fn metadata_listing_is_floored_by_min_interval() {
        // The verbose listing is heavy on the source, so it is rate-floored. A
        // poll inside the floor defers the *additions* — and says so, rather than
        // presenting a short set as complete.
        let mut config = fast_config();
        config.set_metadata_min_interval(Duration::from_secs(30));
        let (service, subscriber, source) = spawn_core(100, 0xAB, vec![mtx(1, 100)], config);

        wait_for(&subscriber, |s| s.tx_count() == 1).await;
        let listings_after_startup = source.metadata_fetch_count();

        source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            source.metadata_fetch_count(),
            listings_after_startup,
            "listing re-issued inside the floor"
        );
        let snapshot = subscriber.snapshot();
        assert_eq!(snapshot.tx_count(), 1);
        assert_eq!(
            snapshot.completeness(),
            MempoolCompleteness::IncompletePendingMetadata,
            "a deferred addition must be reported as such, not as a complete set"
        );
        assert!(snapshot.unadmitted().contains(&txid(2)));
        service.close();
    }

    #[tokio::test]
    async fn metadata_deferral_still_publishes_removals_and_retag() {
        // Deferring additions must not withhold the rest of the poll: coherence
        // thaws on the tip re-tag, so holding it back would make
        // `metadata_min_interval` extend the post-block freeze by its own length.
        let mut config = fast_config();
        config.set_metadata_min_interval(Duration::from_secs(30));
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], config);

        wait_for(&subscriber, |s| s.tx_count() == 2).await;

        // One transaction leaves, another arrives, and the tip advances — all in
        // the same poll, with the metadata listing floored out.
        source.set_mempool(vec![mtx(2, 100), mtx(3, 100)]);
        source.set_tip(block_ref(101, 0xCD));

        let snapshot = wait_for(&subscriber, |s| {
            s.source_tip() == Some(block_ref(101, 0xCD))
        })
        .await;
        assert!(
            !snapshot.by_txid().contains_key(&txid(1)),
            "the removal must apply even though the additions were deferred"
        );
        assert!(
            !snapshot.by_txid().contains_key(&txid(3)),
            "the addition must still be deferred"
        );
        assert!(snapshot.unadmitted().contains(&txid(3)));
        service.close();
    }

    #[tokio::test]
    async fn get_mempool_info_reports_totals() {
        let (service, subscriber, _source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], fast_config());
        wait_for(&subscriber, |s| s.tx_count() == 2).await;

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
        wait_for(&subscriber, |s| s.tx_count() == 2).await;

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
        wait_for(&subscriber, |s| s.tx_count() == 1).await;

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
            s.completeness() == MempoolCompleteness::Complete
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
        config.set_event_buffer_len(NonZeroUsize::new(2).expect("non-zero test buffer")); // tiny buffer: one publish overflows it

        let source = MockSource::new();
        source.set_tip(block_ref(100, 0xAB));
        let service = MempoolService::spawn(source.clone(), config, CancellationToken::new());
        let subscriber = service.subscriber();
        wait_for(&subscriber, |s| {
            s.completeness() == MempoolCompleteness::Complete
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
        wait_for(&subscriber, |s| s.tx_count() == 200).await;

        let mut handles = Vec::new();
        for _ in 0..16 {
            let sub = subscriber.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..500 {
                    let snap = sub.snapshot();
                    assert_eq!(snap.tx_count(), snap.by_txid().len());
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
#[cfg(feature = "tip_aware_mempool")]
mod coherence {
    use super::*;

    use crate::{CoherenceService, CoherentSubscriber, MempoolService, MempoolSubscriber};
    use futures::StreamExt as _;
    use zaino_mempool::ports::MempoolStreamError;
    use zaino_mempool::tip::{CoherentSnapshot, FreezeReason, MempoolMode};
    use zaino_mempool::TipAwareMempool as _;

    /// Keep the core service alive alongside the coherence layer under test.
    struct Harness {
        core: Arc<MempoolService<MockSource>>,
        coherence: Arc<CoherenceService<MempoolSubscriber, MockNfs>>,
        subscriber: CoherentSubscriber,
        source: MockSource,
        nfs: MockNfs,
    }

    impl Harness {
        fn close(&self) {
            self.coherence.close();
            self.core.close();
        }
    }

    fn spawn_coherent(
        height: u32,
        hash_byte: u8,
        generation: u64,
        mempool: Vec<MockTx>,
        config: MempoolConfig,
    ) -> Harness {
        let source = MockSource::new();
        let nfs = MockNfs::new();
        source.set_tip(block_ref(height, hash_byte));
        source.set_mempool(mempool);
        nfs.set(epoch(generation, height, hash_byte));

        let core = MempoolService::spawn(source.clone(), config.clone(), CancellationToken::new());
        let coherence = CoherenceService::spawn(
            core.subscriber(),
            nfs.clone(),
            config,
            CancellationToken::new(),
        );
        let subscriber = coherence.subscriber();
        Harness {
            core,
            coherence,
            subscriber,
            source,
            nfs,
        }
    }

    /// As `core::wait_for`; see there for why the budget is what it is.
    async fn wait_for(
        subscriber: &CoherentSubscriber,
        predicate: impl Fn(&CoherentSnapshot) -> bool,
    ) -> Arc<CoherentSnapshot> {
        for _ in 0..2000 {
            let snapshot = subscriber.coherent_snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("coherent snapshot never satisfied the predicate");
    }

    fn is_live(snapshot: &CoherentSnapshot) -> bool {
        matches!(snapshot.mode, MempoolMode::Live { .. })
    }

    fn is_frozen(snapshot: &CoherentSnapshot) -> bool {
        matches!(snapshot.mode, MempoolMode::Frozen { .. })
    }

    fn freeze_reason(snapshot: &CoherentSnapshot) -> Option<FreezeReason> {
        match snapshot.mode {
            MempoolMode::Frozen { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// Waits for a freeze *with this reason*, rather than for any freeze.
    ///
    /// Waiting on [`is_frozen`] and then asserting the reason races the core's
    /// first publish: until the core has a set, the coherent view is already
    /// frozen — for `CoreIncomplete`, not for whatever the test set up. The wait
    /// would return that startup freeze and the assertion would fail on a
    /// perfectly correct service. Folding the reason into the predicate makes
    /// the wait say what the test means, and turns a genuine failure into a
    /// timeout naming the reason that never arrived.
    fn frozen_because(reason: FreezeReason) -> impl Fn(&CoherentSnapshot) -> bool {
        move |snapshot| freeze_reason(snapshot) == Some(reason)
    }

    #[tokio::test]
    async fn nfs_advance_reconciles_on_the_signal_not_the_tick() {
        // The coherence layer must learn of an NS advance from the observer's
        // signal. Waiting for its own tick means tip-coherent reads stay frozen
        // for a whole poll interval after every block — indefinitely when sync
        // lags. The poll interval here is far longer than the test's patience,
        // so only the signal can satisfy it.
        let source = MockSource::new();
        let nfs = MockNfs::new();
        source.set_tip(block_ref(100, 0xAB));
        source.set_mempool(vec![mtx(1, 100)]);
        nfs.set(epoch(1, 100, 0xAB));

        let mut slow_tick = fast_config();
        slow_tick.set_poll_interval_ms(NonZeroU64::new(30_000).expect("non-zero test interval"));

        let core = MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let coherence = CoherenceService::spawn(
            core.subscriber(),
            nfs.clone(),
            slow_tick,
            CancellationToken::new(),
        );
        let subscriber = coherence.subscriber();
        wait_for(&subscriber, is_live).await;

        // Advance *only* the NS epoch. The validator tip and the mempool set are
        // untouched, so the core publishes nothing and its change feed cannot be
        // what wakes the layer — the observer's signal is the only path left.
        nfs.set(epoch(2, 101, 0xCD));

        wait_for(&subscriber, frozen_because(FreezeReason::TipsDiverged)).await;

        // Without the signal this state change would wait out the 30s tick;
        // arriving inside the wait above is the assertion.
        coherence.close();
        core.close();
    }

    #[tokio::test]
    async fn agreement_publishes_a_live_coherent_view() {
        let h = spawn_coherent(100, 0xAB, 7, vec![mtx(1, 100), mtx(2, 100)], fast_config());

        let snapshot = wait_for(&h.subscriber, is_live).await;
        assert!(snapshot.is_live_for(epoch(7, 100, 0xAB)));
        assert_eq!(snapshot.set.tx_count(), 2);
        assert!(snapshot.is_valid_for_snapshot(epoch(7, 100, 0xAB)));
        assert!(snapshot.get(&txid(1)).is_some());
        h.close();
    }

    #[tokio::test]
    async fn validator_tip_change_freezes_and_preserves_transactions() {
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&h.subscriber, is_live).await;

        h.source.set_tip(block_ref(101, 0xCD)); // V advances, NS stays

        let snapshot = wait_for(&h.subscriber, is_frozen).await;
        assert_eq!(snapshot.set.tx_count(), 1); // last coherent set stays readable
        assert!(snapshot.set.by_txid().contains_key(&txid(1)));
        assert_eq!(snapshot.valid_for, Some(epoch(1, 100, 0xAB)));
        assert_eq!(freeze_reason(&snapshot), Some(FreezeReason::TipsDiverged));
        h.close();
    }

    #[tokio::test]
    async fn tip_agnostic_core_stays_live_while_coherence_freezes() {
        // The payoff of the split: during a tip transition the tip-agnostic core
        // keeps serving the *live* validator mempool (GetMempoolTx / getrawmempool)
        // while only the tip-coherent view freezes.
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        let core = h.core.subscriber();
        wait_for(&h.subscriber, is_live).await;

        // The validator advances to a new tip and its mempool gains a transaction;
        // Zaino's NS tip stays behind (V != NS).
        h.source.set_tip(block_ref(101, 0xCD));
        h.source.set_mempool(vec![mtx(1, 100), mtx(2, 101)]);

        // The core reflects the new live mempool immediately — it never freezes.
        // Same 10s starvation allowance as `wait_for`; hand-rolled because this
        // polls the *core* handle while the suite's helper polls the coherent one.
        let mut live = core.snapshot();
        for _ in 0..2000 {
            live = core.snapshot();
            if live.tx_count() == 2 && live.source_tip() == Some(block_ref(101, 0xCD)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            live.tx_count(),
            2,
            "core must serve the live mempool during a transition"
        );
        assert_eq!(
            live.completeness(),
            zaino_mempool::snapshot::MempoolCompleteness::Complete
        );
        assert!(live.by_txid().contains_key(&txid(2)));

        // Meanwhile the coherent view freezes at the last coherent set: the new
        // transaction is live in the core but not blessed for the stale NS tip.
        let frozen = wait_for(&h.subscriber, frozen_because(FreezeReason::TipsDiverged)).await;
        assert_eq!(frozen.set.tx_count(), 1);
        assert!(!frozen.set.by_txid().contains_key(&txid(2)));

        h.close();
    }

    #[tokio::test]
    async fn nonfinalized_tip_change_freezes() {
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&h.subscriber, is_live).await;

        h.nfs.set(epoch(2, 101, 0xCD)); // NS advances, V stays

        let snapshot = wait_for(&h.subscriber, frozen_because(FreezeReason::TipsDiverged)).await;
        assert_eq!(snapshot.set.tx_count(), 1);
        h.close();
    }

    #[tokio::test]
    async fn agreement_after_divergence_thaws_to_live() {
        let source = MockSource::new();
        let nfs = MockNfs::new();
        source.set_tip(block_ref(100, 0xAA));
        source.set_mempool(vec![mtx(1, 100)]);
        nfs.set(epoch(1, 100, 0xBB)); // diverged

        let core = MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let coherence = CoherenceService::spawn(
            core.subscriber(),
            nfs.clone(),
            fast_config(),
            CancellationToken::new(),
        );
        let subscriber = coherence.subscriber();

        wait_for(&subscriber, frozen_because(FreezeReason::TipsDiverged)).await;

        nfs.set(epoch(2, 100, 0xAA)); // agree

        let snapshot = wait_for(&subscriber, is_live).await;
        assert_eq!(snapshot.set.tx_count(), 1);
        assert!(snapshot.is_live_for(epoch(2, 100, 0xAA)));
        coherence.close();
        core.close();
    }

    #[tokio::test]
    async fn coherence_thaw_latency_unaffected_by_metadata_interval() {
        // R2: a long `metadata_min_interval` defers additions, but the poll still
        // publishes its removals and tip re-tag — and coherence thaws on the
        // re-tag. So after a block the coherent view returns to `Live` on the
        // poll cadence, *not* after the metadata interval. The interval here is
        // far longer than the wait budget: if thaw waited for it, the wait times
        // out and the test fails.
        let mut config = fast_config();
        config.set_metadata_min_interval(Duration::from_secs(10));
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], config);
        wait_for(&h.subscriber, is_live).await;

        // A new block: validator tip and NS both advance, and a new transaction
        // enters the mempool (an addition that needs metadata, so it defers).
        h.source.set_tip(block_ref(101, 0xCD));
        h.source.set_mempool(vec![mtx(1, 100), mtx(2, 101)]);
        h.nfs.set(epoch(2, 101, 0xCD));

        // Thaws to Live for the new epoch within the wait budget (~5s), well
        // inside the 10s metadata interval.
        let snapshot = wait_for(&h.subscriber, |s| s.is_live_for(epoch(2, 101, 0xCD))).await;

        // The re-tag carried the new tip; the addition is deferred, not admitted,
        // and the set says so — which is exactly why thaw did not wait for it.
        assert_eq!(snapshot.set.source_tip(), Some(block_ref(101, 0xCD)));
        assert!(!snapshot.set.by_txid().contains_key(&txid(2)));
        assert_eq!(
            snapshot.set.completeness(),
            zaino_mempool::snapshot::MempoolCompleteness::IncompletePendingMetadata
        );

        h.close();
    }

    #[tokio::test]
    async fn missing_nonfinalized_state_stays_not_ready() {
        let source = MockSource::new();
        let nfs = MockNfs::new(); // never set: NS unavailable
        source.set_tip(block_ref(100, 0xAA));
        source.set_mempool(vec![mtx(1, 100)]);

        let core = MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let coherence = CoherenceService::spawn(
            core.subscriber(),
            nfs.clone(),
            fast_config(),
            CancellationToken::new(),
        );
        let subscriber = coherence.subscriber();

        tokio::time::sleep(Duration::from_millis(80)).await;
        let snapshot = subscriber.coherent_snapshot();
        assert!(!is_live(&snapshot));
        assert_eq!(snapshot.valid_for, None);
        // Core is ready with data; coherence freezes on the missing NS tip.
        assert_eq!(
            freeze_reason(&snapshot),
            Some(FreezeReason::NonFinalizedUnavailable)
        );
        coherence.close();
        core.close();
    }

    #[tokio::test]
    async fn core_source_error_freezes_coherent_view() {
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&h.subscriber, is_live).await;

        h.source.set_error(Some("validator unreachable"));

        let snapshot = wait_for(&h.subscriber, frozen_because(FreezeReason::CoreIncomplete)).await;
        assert_eq!(snapshot.set.tx_count(), 1); // last coherent set preserved
        h.close();
    }

    #[tokio::test]
    async fn capacity_limited_set_still_serves_coherent_reads() {
        // A set that is *short* (capacity-refused) but tip-consistent is an
        // accurate view of what it holds, so it must serve `Live` — freezing
        // would withhold the transactions Zaino does have on top of the ones it
        // doesn't. Only a set that may be *wrong* (source error) freezes.
        let config = fast_config();
        config.set_max_cost_bytes(1); // below the ZIP-401 floor: every addition refused
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100), mtx(2, 100)], config);

        // Short but blessed: the view is Live even though the set is incomplete.
        let live = wait_for(&h.subscriber, is_live).await;
        assert!(
            !live.set.completeness().is_whole(),
            "the capacity-bounded set must be short, got {:?}",
            live.set.completeness()
        );
        assert_eq!(
            live.set.completeness(),
            zaino_mempool::snapshot::MempoolCompleteness::IncompleteCapacityLimited
        );
        assert!(
            !live.set.unadmitted().is_empty(),
            "a capacity-refused set must name the txids it is short of"
        );

        // A set that may be wrong (source error) still freezes.
        h.source.set_error(Some("validator unreachable"));
        wait_for(&h.subscriber, frozen_because(FreezeReason::CoreIncomplete)).await;

        h.close();
    }

    #[tokio::test]
    async fn freeze_duration_is_tracked_and_cleared_on_thaw() {
        // The freeze clock backing the N2(e) escalation signal: `None` while
        // serving, `Some` while frozen, and back to `None` once thawed.
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&h.subscriber, is_live).await;
        assert!(
            h.subscriber.frozen_for().is_none(),
            "a live view is not frozen"
        );

        h.nfs.set(epoch(2, 101, 0xCD)); // NS advances, V stays: freeze
        wait_for(&h.subscriber, is_frozen).await;
        assert!(
            h.subscriber.frozen_for().is_some(),
            "a frozen view must report a freeze duration"
        );

        h.nfs.set(epoch(3, 100, 0xAB)); // re-agree with V: thaw
        wait_for(&h.subscriber, is_live).await;
        assert!(
            h.subscriber.frozen_for().is_none(),
            "the freeze clock must clear on thaw"
        );

        h.close();
    }

    #[tokio::test]
    async fn stream_yields_initial_then_added() {
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&h.subscriber, |s| is_live(s) && s.set.tx_count() == 1).await;

        let mut stream = Box::pin(
            h.subscriber
                .stream_transactions_until_tip_change(Some(epoch(1, 100, 0xAB)))
                .expect("coherent for this epoch"),
        );

        // Initial set: the one transaction's bytes.
        assert_eq!(
            stream.next().await,
            Some(Ok(bytes::Bytes::from_static(&[1])))
        );

        // Add a second transaction at the same epoch: it streams live.
        h.source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
        assert_eq!(
            stream.next().await,
            Some(Ok(bytes::Bytes::from_static(&[2])))
        );
        h.close();
    }

    #[tokio::test]
    async fn stream_surfaces_a_lag_as_an_error_not_a_silent_end() {
        // A stream consumer that falls behind must be told. Ending silently is
        // indistinguishable from the normal tip-change close, so the client
        // would treat a partial mempool as the complete one.
        let mut config = fast_config();
        config.set_event_buffer_len(NonZeroUsize::new(2).expect("non-zero test buffer")); // tiny buffer: a small flood overflows it

        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], config);
        wait_for(&h.subscriber, |s| is_live(s) && s.set.tx_count() == 1).await;

        let mut stream = Box::pin(
            h.subscriber
                .stream_transactions_until_tip_change(Some(epoch(1, 100, 0xAB)))
                .expect("coherent for this epoch"),
        );
        // Drain the initial set so the next item comes from the event feed.
        assert_eq!(
            stream.next().await,
            Some(Ok(bytes::Bytes::from_static(&[1])))
        );

        // Flood far more coherent events than the buffer holds, without polling.
        let flood: Vec<MockTx> = (0..50)
            .map(|n| MockTx {
                txid: txid_n(n),
                entry_height: 100,
                entry_time: None,
                bytes: vec![(n % 251) as u8],
            })
            .collect();
        h.source.set_mempool(flood);
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Collect until the stream ends; a lag must appear as an error item.
        let mut saw_lag = false;
        while let Some(item) = tokio::time::timeout(Duration::from_millis(200), stream.next())
            .await
            .unwrap_or(None)
        {
            if matches!(item, Err(MempoolStreamError::Lagged { .. })) {
                saw_lag = true;
                break;
            }
        }
        assert!(saw_lag, "lag ended the stream without surfacing an error");
        h.close();
    }

    #[tokio::test]
    async fn stream_is_none_for_stale_epoch() {
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&h.subscriber, is_live).await;

        // A caller whose epoch does not match the coherent view gets `None`.
        assert!(h
            .subscriber
            .stream_transactions_until_tip_change(Some(epoch(9, 999, 0xFF)))
            .is_none());
        h.close();
    }

    #[tokio::test]
    async fn validator_only_freezes_on_tip_change_and_thaws() {
        // No NS observer: the epoch is synthesized from the validator tip, so a
        // stable tip is live and a tip change is a single-tip freeze/thaw.
        let source = MockSource::new();
        source.set_tip(block_ref(100, 0xAA));
        source.set_mempool(vec![mtx(1, 100)]);
        let core = MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let coherence = CoherenceService::spawn_validator_only(
            core.subscriber(),
            fast_config(),
            CancellationToken::new(),
        );
        let subscriber = coherence.subscriber();

        let live = wait_for(&subscriber, is_live).await;
        assert_eq!(live.set.tx_count(), 1);

        source.set_tip(block_ref(101, 0xBB)); // tip change: re-synthesize, stay live
        let snapshot = wait_for(&subscriber, |s| {
            is_live(s) && s.observed_tips.validator == Some(block_ref(101, 0xBB))
        })
        .await;
        assert!(is_live(&snapshot));
        coherence.close();
        core.close();
    }

    #[tokio::test]
    async fn coherent_empty_not_ready() {
        let snapshot = CoherentSnapshot::empty_not_ready();
        assert!(matches!(snapshot.mode, MempoolMode::NotReady));
        assert_eq!(snapshot.valid_for, None);
        assert_eq!(snapshot.set.tx_count(), 0);
    }

    /// The pre-first-poll set must never be blessed as coherent.
    ///
    /// It is empty, so serving it would answer "your transaction is not
    /// pending" when Zaino has merely not looked yet — indistinguishable, to a
    /// caller, from a real absence. This was previously guaranteed by
    /// `MempoolCompleteness::NotReady` making `may_be_wrong()` true; readiness
    /// now lives on `MempoolSnapshot::is_ready`, and the guarantee is pinned
    /// here so it cannot be lost with the variant.
    #[tokio::test]
    async fn an_unpolled_set_is_never_served_as_coherent() {
        let source = MockSource::new();
        let nfs = MockNfs::new();
        // A tip and a mempool exist on the NS side, so the *only* thing keeping
        // this from going live is that the core has not polled the source yet.
        nfs.set(epoch(1, 100, 0xAB));

        let core = MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let coherence = CoherenceService::spawn(
            core.subscriber(),
            nfs.clone(),
            fast_config(),
            CancellationToken::new(),
        );
        let subscriber = coherence.subscriber();

        assert!(
            !core.subscriber().snapshot().is_ready(),
            "a source with no tip cannot produce a ready set"
        );

        // Give the loops room to run: the point is that they never go live.
        tokio::time::sleep(Duration::from_millis(80)).await;

        let snapshot = subscriber.coherent_snapshot();
        assert!(
            !is_live(&snapshot),
            "an unpolled mempool must not be served as a coherent view, got {:?}",
            snapshot.mode
        );
        assert_eq!(snapshot.valid_for, None);

        coherence.close();
        core.close();
    }

    /// Before the first poll the freeze reason names the missing validator tip,
    /// not the core's completeness.
    ///
    /// The set is empty and `Complete` — there is nothing wrong with it, there
    /// is simply no tip to place it against — so `CoreIncomplete` would have
    /// pointed an operator at the wrong thing.
    #[tokio::test]
    async fn the_startup_freeze_names_the_missing_tip() {
        let source = MockSource::new();
        let nfs = MockNfs::new();
        nfs.set(epoch(1, 100, 0xAB));

        let core = MempoolService::spawn(source.clone(), fast_config(), CancellationToken::new());
        let coherence = CoherenceService::spawn(
            core.subscriber(),
            nfs.clone(),
            fast_config(),
            CancellationToken::new(),
        );
        let subscriber = coherence.subscriber();

        let snapshot = wait_for(&subscriber, is_frozen).await;
        assert_eq!(
            freeze_reason(&snapshot),
            Some(FreezeReason::ValidatorTipUnavailable)
        );

        coherence.close();
        core.close();
    }

    #[test]
    fn observed_tips_agree_and_disagree() {
        use zaino_mempool::tip::ObservedTips;

        let v = block_ref(100, 0xAB);
        let ns_same = epoch(1, 100, 0xAB);
        let ns_diff = epoch(1, 100, 0xCD);

        assert_eq!(ObservedTips::none().agree(), None);
        assert!(!ObservedTips::none().disagree());

        let only_v = ObservedTips {
            validator: Some(v),
            non_finalized: None,
        };
        assert_eq!(only_v.agree(), None);
        assert!(!only_v.disagree());

        let agree = ObservedTips {
            validator: Some(v),
            non_finalized: Some(ns_same),
        };
        assert_eq!(agree.agree(), Some(ns_same));

        let disagree = ObservedTips {
            validator: Some(v),
            non_finalized: Some(ns_diff),
        };
        assert_eq!(disagree.agree(), None);
        assert!(disagree.disagree());
    }
}
