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
use zebra_chain::{
    block::{Hash as BlockHash, Height},
    transaction::{Hash as TxHash, SerializedTransaction},
};

use zaino_mempool::config::MempoolConfig;
use zaino_mempool::ports::{BlockRef, MempoolSource, MempoolTxMeta};
use zaino_mempool::MempoolError;

#[cfg(feature = "tip_aware_mempool")]
use zaino_mempool::ports::{NfsEpochObserver, NonFinalizedEpoch};

// ---- mock ports --------------------------------------------------------

#[derive(Clone)]
struct MockTx {
    txid: TxHash,
    entry_height: u32,
    entry_time: Option<i64>,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct MockSourceState {
    tip: Option<BlockRef>,
    mempool: Vec<MockTx>,
    /// Txids listed by metadata/txids but whose raw fetch returns `None`.
    phantom: HashSet<TxHash>,
    raw_fetch_counts: HashMap<TxHash, usize>,
    /// Number of verbose metadata listings served.
    metadata_fetch_count: usize,
    /// If set, source calls fail with this message (a source outage).
    source_error: Option<String>,
}

#[derive(Clone)]
struct MockSource {
    state: Arc<Mutex<MockSourceState>>,
}

impl MockSource {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockSourceState::default())),
        }
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

    fn raw_fetch_count(&self, txid: &TxHash) -> usize {
        self.lock().raw_fetch_counts.get(txid).copied().unwrap_or(0)
    }

    fn metadata_fetch_count(&self) -> usize {
        self.lock().metadata_fetch_count
    }

    fn error(&self) -> Option<MempoolError> {
        self.lock()
            .source_error
            .clone()
            .map(|message| MempoolError::source(std::io::Error::other(message)))
    }
}

impl MempoolSource for MockSource {
    async fn get_mempool_txids(&self) -> Result<Option<Vec<TxHash>>, MempoolError> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        Ok(Some(self.lock().mempool.iter().map(|tx| tx.txid).collect()))
    }

    async fn get_mempool_metadata(&self) -> Result<Option<Vec<MempoolTxMeta>>, MempoolError> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        self.lock().metadata_fetch_count += 1;
        Ok(Some(
            self.lock()
                .mempool
                .iter()
                .map(|tx| MempoolTxMeta {
                    txid: tx.txid,
                    entry_height: Height(tx.entry_height),
                    entry_time: tx.entry_time,
                })
                .collect(),
        ))
    }

    async fn get_raw_mempool_transaction(
        &self,
        txid: TxHash,
    ) -> Result<Option<SerializedTransaction>, MempoolError> {
        let mut state = self.lock();
        *state.raw_fetch_counts.entry(txid).or_default() += 1;
        if state.phantom.contains(&txid) {
            return Ok(None);
        }
        Ok(state
            .mempool
            .iter()
            .find(|tx| tx.txid == txid)
            .map(|tx| SerializedTransaction::from(tx.bytes.clone())))
    }

    async fn get_mempool_source_tip(&self) -> Result<Option<BlockRef>, MempoolError> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        Ok(self.lock().tip)
    }
}

#[cfg(feature = "tip_aware_mempool")]
#[derive(Clone)]
struct MockNfs {
    epoch: Arc<Mutex<Option<NonFinalizedEpoch>>>,
}

#[cfg(feature = "tip_aware_mempool")]
impl MockNfs {
    fn new() -> Self {
        Self {
            epoch: Arc::new(Mutex::new(None)),
        }
    }

    fn set(&self, epoch: NonFinalizedEpoch) {
        *self.epoch.lock().expect("mock nfs poisoned") = Some(epoch);
    }
}

#[cfg(feature = "tip_aware_mempool")]
impl NfsEpochObserver for MockNfs {
    fn current_epoch(&self) -> Option<NonFinalizedEpoch> {
        *self.epoch.lock().expect("mock nfs poisoned")
    }
}

// ---- helpers -----------------------------------------------------------

fn txid(byte: u8) -> TxHash {
    TxHash([byte; 32])
}

/// A distinct txid derived from an index (for large-mempool tests).
fn txid_n(n: u32) -> TxHash {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    TxHash(bytes)
}

fn mtx(byte: u8, entry_height: u32) -> MockTx {
    MockTx {
        txid: txid(byte),
        entry_height,
        entry_time: None,
        bytes: vec![byte],
    }
}

fn block_ref(height: u32, hash_byte: u8) -> BlockRef {
    BlockRef {
        height: Height(height),
        hash: BlockHash([hash_byte; 32]),
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

    async fn wait_for(
        subscriber: &MempoolSubscriber,
        predicate: impl Fn(&MempoolSnapshot) -> bool,
    ) -> Arc<MempoolSnapshot> {
        for _ in 0..1000 {
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
        assert_eq!(snapshot.by_txid[&txid(1)].entry_height, Height(100));

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
    async fn refused_transactions_are_not_refetched_every_poll() {
        // A transaction refused by the capacity backstop must be remembered, not
        // rediscovered by the next diff and re-fetched forever.
        let config = fast_config();
        config.set_max_cost_bytes(zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD);
        let (service, subscriber, source) =
            spawn_core(100, 0xAB, vec![mtx(1, 100), mtx(2, 100)], config);

        wait_for(&subscriber, |s| {
            s.completeness == MempoolCompleteness::IncompleteCapacityLimited
        })
        .await;

        // Many poll intervals pass; the refused transaction is never re-fetched.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let fetches = source.raw_fetch_count(&txid(1)) + source.raw_fetch_count(&txid(2));
        assert_eq!(fetches, 2, "each transaction fetched exactly once");
        service.close();
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
        // poll that finds additions before the floor elapses must publish
        // *nothing* — never the set without them, which would present an
        // incomplete view as complete.
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
        assert_eq!(snapshot.completeness, MempoolCompleteness::Complete);
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
                txid: TxHash(a),
                entry_height: 100,
                entry_time: None,
                bytes: vec![1],
            },
            MockTx {
                txid: TxHash(b),
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
        assert_eq!(kept[0].txid, TxHash(b));
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

#[cfg(feature = "tip_aware_mempool")]
mod coherence {
    use super::*;

    use crate::{CoherenceService, CoherentSubscriber, MempoolService, MempoolSubscriber};
    use futures::StreamExt as _;
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

    async fn wait_for(
        subscriber: &CoherentSubscriber,
        predicate: impl Fn(&CoherentSnapshot) -> bool,
    ) -> Arc<CoherentSnapshot> {
        for _ in 0..1000 {
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

    #[tokio::test]
    async fn agreement_publishes_a_live_coherent_view() {
        let h = spawn_coherent(100, 0xAB, 7, vec![mtx(1, 100), mtx(2, 100)], fast_config());

        let snapshot = wait_for(&h.subscriber, is_live).await;
        assert!(snapshot.is_live_for(epoch(7, 100, 0xAB)));
        assert_eq!(snapshot.set.tx_count, 2);
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
        assert_eq!(snapshot.set.tx_count, 1); // last coherent set stays readable
        assert!(snapshot.set.by_txid.contains_key(&txid(1)));
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
        let mut live = core.snapshot();
        for _ in 0..1000 {
            live = core.snapshot();
            if live.tx_count == 2 && live.source_tip == Some(block_ref(101, 0xCD)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            live.tx_count, 2,
            "core must serve the live mempool during a transition"
        );
        assert_eq!(
            live.completeness,
            zaino_mempool::snapshot::MempoolCompleteness::Complete
        );
        assert!(live.by_txid.contains_key(&txid(2)));

        // Meanwhile the coherent view freezes at the last coherent set: the new
        // transaction is live in the core but not blessed for the stale NS tip.
        let frozen = wait_for(&h.subscriber, is_frozen).await;
        assert_eq!(freeze_reason(&frozen), Some(FreezeReason::TipsDiverged));
        assert_eq!(frozen.set.tx_count, 1);
        assert!(!frozen.set.by_txid.contains_key(&txid(2)));

        h.close();
    }

    #[tokio::test]
    async fn nonfinalized_tip_change_freezes() {
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&h.subscriber, is_live).await;

        h.nfs.set(epoch(2, 101, 0xCD)); // NS advances, V stays

        let snapshot = wait_for(&h.subscriber, is_frozen).await;
        assert_eq!(snapshot.set.tx_count, 1);
        assert_eq!(freeze_reason(&snapshot), Some(FreezeReason::TipsDiverged));
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

        let frozen = wait_for(&subscriber, is_frozen).await;
        assert_eq!(freeze_reason(&frozen), Some(FreezeReason::TipsDiverged));

        nfs.set(epoch(2, 100, 0xAA)); // agree

        let snapshot = wait_for(&subscriber, is_live).await;
        assert_eq!(snapshot.set.tx_count, 1);
        assert!(snapshot.is_live_for(epoch(2, 100, 0xAA)));
        coherence.close();
        core.close();
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

        let snapshot = wait_for(&h.subscriber, is_frozen).await;
        assert_eq!(freeze_reason(&snapshot), Some(FreezeReason::CoreIncomplete));
        assert_eq!(snapshot.set.tx_count, 1); // last coherent set preserved
        h.close();
    }

    #[tokio::test]
    async fn stream_yields_initial_then_added() {
        let h = spawn_coherent(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&h.subscriber, |s| is_live(s) && s.set.tx_count == 1).await;

        let mut stream = Box::pin(
            h.subscriber
                .stream_transactions_until_tip_change(Some(epoch(1, 100, 0xAB)))
                .expect("coherent for this epoch"),
        );

        // Initial set: the one transaction's bytes.
        assert_eq!(stream.next().await, Some(vec![1]));

        // Add a second transaction at the same epoch: it streams live.
        h.source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
        assert_eq!(stream.next().await, Some(vec![2]));
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
        assert_eq!(live.set.tx_count, 1);

        source.set_tip(block_ref(101, 0xBB)); // tip change: re-synthesize, stay live
        let snapshot = wait_for(&subscriber, |s| {
            is_live(s)
                && s.observed_tips.validator
                    == Some(zaino_mempool::tip::ValidatorTip {
                        best_tip: block_ref(101, 0xBB),
                    })
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
        assert_eq!(snapshot.set.tx_count, 0);
    }

    #[test]
    fn observed_tips_agree_and_disagree() {
        use zaino_mempool::tip::{ObservedTips, ValidatorTip};

        let v = ValidatorTip {
            best_tip: block_ref(100, 0xAB),
        };
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
