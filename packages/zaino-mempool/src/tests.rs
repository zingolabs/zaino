//! The mempool service test matrix, driven by in-crate mock ports.
//!
//! Covers the freeze/thaw tip-coherence rules, incremental update behaviour,
//! capacity bounds, source errors, frozen serving, stream/subscriber semantics,
//! validator-only mode, and (in the `concurrency` submodule) high-throughput /
//! many-reader behaviour. Epoch-gated *combined* ChainIndex reads
//! (`get_raw_transaction` etc.) are covered by `zaino-state`'s integration tests.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;
use zebra_chain::{
    block::{Hash as BlockHash, Height},
    transaction::{Hash as TxHash, SerializedTransaction},
};

use crate::config::MempoolConfig;
use crate::event::MempoolEvent;
use crate::ports::{BlockRef, MempoolSource, MempoolTxMeta, NfsEpochObserver, NonFinalizedEpoch};
use crate::snapshot::{FreezeReason, MempoolCompleteness, MempoolMode, MempoolSnapshot};
use crate::subscriber::MempoolSubscriber;
use crate::{MempoolError, MempoolService};

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
    /// If set, the next raw fetch advances the tip (once), racing the update.
    advance_tip_on_next_fetch: Option<BlockRef>,
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
        if let Some(new_tip) = state.advance_tip_on_next_fetch.take() {
            state.tip = Some(new_tip);
        }
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

#[derive(Clone)]
struct MockNfs {
    epoch: Arc<Mutex<Option<NonFinalizedEpoch>>>,
}

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

fn epoch(generation: u64, height: u32, hash_byte: u8) -> NonFinalizedEpoch {
    NonFinalizedEpoch {
        generation,
        best_tip: block_ref(height, hash_byte),
    }
}

fn fast_config() -> MempoolConfig {
    let mut config = MempoolConfig::default();
    config.poll_interval = Duration::from_millis(5);
    config
}

fn spawn_agreeing(
    height: u32,
    hash_byte: u8,
    generation: u64,
    mempool: Vec<MockTx>,
    config: MempoolConfig,
) -> (
    Arc<MempoolService<MockSource, MockNfs>>,
    MempoolSubscriber,
    MockSource,
    MockNfs,
) {
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(height, hash_byte));
    source.set_mempool(mempool);
    nfs.set(epoch(generation, height, hash_byte));

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        config,
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();
    (service, subscriber, source, nfs)
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
    panic!("mempool snapshot never satisfied the predicate");
}

fn is_live(snapshot: &MempoolSnapshot) -> bool {
    matches!(snapshot.mode, MempoolMode::Live { .. })
}

fn is_frozen(snapshot: &MempoolSnapshot) -> bool {
    matches!(snapshot.mode, MempoolMode::Frozen { .. })
}

fn freeze_reason(snapshot: &MempoolSnapshot) -> Option<FreezeReason> {
    match snapshot.mode {
        MempoolMode::Frozen { reason, .. } => Some(reason),
        _ => None,
    }
}

// ---- pure units --------------------------------------------------------

#[test]
fn observed_tips_agree_and_disagree() {
    use crate::snapshot::{ObservedTips, ValidatorTip};

    let v = ValidatorTip {
        best_tip: block_ref(100, 0xAB),
    };
    let ns_same = epoch(1, 100, 0xAB);
    let ns_diff = epoch(1, 100, 0xCD);

    // Both unknown.
    let none = ObservedTips::none();
    assert_eq!(none.agree(), None);
    assert!(!none.disagree());

    // One unknown.
    let only_v = ObservedTips {
        validator: Some(v),
        non_finalized: None,
    };
    assert_eq!(only_v.agree(), None);
    assert!(!only_v.disagree());

    // Agree (same hash).
    let agree = ObservedTips {
        validator: Some(v),
        non_finalized: Some(ns_same),
    };
    assert_eq!(agree.agree(), Some(ns_same));
    assert!(!agree.disagree());

    // Disagree (different hash).
    let disagree = ObservedTips {
        validator: Some(v),
        non_finalized: Some(ns_diff),
    };
    assert_eq!(disagree.agree(), None);
    assert!(disagree.disagree());
}

#[test]
fn empty_not_ready_snapshot() {
    let snapshot = MempoolSnapshot::empty_not_ready();
    assert!(matches!(snapshot.mode, MempoolMode::NotReady));
    assert_eq!(snapshot.completeness, MempoolCompleteness::NotReady);
    assert_eq!(snapshot.valid_for, None);
    assert_eq!(snapshot.tx_count, 0);
    assert!(snapshot.by_txid.is_empty());
}

#[test]
fn mempool_error_source_and_display() {
    let error = MempoolError::source(std::io::Error::other("boom"));
    assert!(matches!(error, MempoolError::Source(_)));
    assert!(error.to_string().contains("boom"));
    assert!(MempoolError::IncorrectChainTip
        .to_string()
        .contains("chain tip"));
}

// ---- tip coherence -----------------------------------------------------

#[tokio::test]
async fn agreement_publishes_a_live_snapshot() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 7, vec![mtx(1, 100), mtx(2, 100)], fast_config());

    let snapshot = wait_for(&subscriber, is_live).await;
    assert!(snapshot.is_live_for(epoch(7, 100, 0xAB)));
    assert_eq!(snapshot.tx_count, 2);
    assert_eq!(snapshot.completeness, MempoolCompleteness::Complete);
    assert_eq!(snapshot.by_txid[&txid(1)].entry_height, Height(100));

    service.close();
}

#[tokio::test]
async fn validator_tip_change_freezes_and_preserves_transactions() {
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    source.set_tip(block_ref(101, 0xCD)); // V advances, NS stays

    let snapshot = wait_for(&subscriber, is_frozen).await;
    assert_eq!(snapshot.tx_count, 1); // last coherent set stays readable
    assert!(snapshot.by_txid.contains_key(&txid(1)));
    assert_eq!(snapshot.valid_for, Some(epoch(1, 100, 0xAB)));
    // V and NS now point at different tip hashes, so the reason is TipsDiverged.
    assert_eq!(freeze_reason(&snapshot), Some(FreezeReason::TipsDiverged));

    service.close();
}

#[tokio::test]
async fn nonfinalized_tip_change_freezes() {
    let (service, subscriber, _source, nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    nfs.set(epoch(2, 101, 0xCD)); // NS advances, V stays

    let snapshot = wait_for(&subscriber, is_frozen).await;
    assert_eq!(snapshot.tx_count, 1);
    // V and NS now disagree, so the reason is TipsDiverged.
    assert_eq!(freeze_reason(&snapshot), Some(FreezeReason::TipsDiverged));
    service.close();
}

#[tokio::test]
async fn agreement_after_divergence_thaws_to_live() {
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(100, 0xAA));
    source.set_mempool(vec![mtx(1, 100)]);
    nfs.set(epoch(1, 100, 0xBB)); // diverged

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    let frozen = wait_for(&subscriber, is_frozen).await;
    assert_eq!(freeze_reason(&frozen), Some(FreezeReason::TipsDiverged));

    nfs.set(epoch(2, 100, 0xAA)); // agree

    let snapshot = wait_for(&subscriber, is_live).await;
    assert_eq!(snapshot.tx_count, 1);
    assert!(snapshot.is_live_for(epoch(2, 100, 0xAA)));
    service.close();
}

#[tokio::test]
async fn missing_nonfinalized_state_stays_not_ready() {
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(100, 0xAA));
    source.set_mempool(vec![mtx(1, 100)]);

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    tokio::time::sleep(Duration::from_millis(80)).await;
    let snapshot = subscriber.snapshot();
    assert!(!is_live(&snapshot));
    assert_eq!(snapshot.tx_count, 0);
    service.close();
}

#[tokio::test]
async fn tip_change_during_fetch_discards_work() {
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(100, 0xAA));
    source.set_mempool(vec![mtx(1, 100)]);
    nfs.set(epoch(1, 100, 0xAA));
    source.lock().advance_tip_on_next_fetch = Some(block_ref(101, 0xCC));

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let snapshot = subscriber.snapshot();
    assert!(!snapshot.is_live_for(epoch(1, 100, 0xAA)));
    assert_eq!(snapshot.tx_count, 0);
    service.close();
}

// ---- source errors -----------------------------------------------------

#[tokio::test]
async fn source_tip_error_freezes_incomplete_source_error() {
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    source.set_error(Some("validator unreachable"));

    let snapshot = wait_for(&subscriber, is_frozen).await;
    assert_eq!(
        snapshot.completeness,
        MempoolCompleteness::IncompleteSourceError
    );
    assert_eq!(freeze_reason(&snapshot), Some(FreezeReason::SourceError));
    assert_eq!(snapshot.tx_count, 1); // prior set preserved
    service.close();
}

// ---- transaction updates ----------------------------------------------

#[tokio::test]
async fn added_transaction_is_fetched_once() {
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100), mtx(2, 100)], fast_config());
    wait_for(&subscriber, |s| s.tx_count == 2).await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(source.raw_fetch_count(&txid(1)), 1);
    assert_eq!(source.raw_fetch_count(&txid(2)), 1);
    service.close();
}

#[tokio::test]
async fn removed_transaction_is_dropped_when_live() {
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100), mtx(2, 100)], fast_config());
    wait_for(&subscriber, |s| s.tx_count == 2).await;

    source.set_mempool(vec![mtx(1, 100)]);

    let snapshot = wait_for(&subscriber, |s| s.tx_count == 1).await;
    assert!(snapshot.by_txid.contains_key(&txid(1)));
    assert!(!snapshot.by_txid.contains_key(&txid(2)));
    service.close();
}

#[tokio::test]
async fn unchanged_set_does_not_republish() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    let live = wait_for(&subscriber, is_live).await;
    let generation = live.mempool_generation;

    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = subscriber.snapshot();
    assert!(is_live(&after));
    assert_eq!(after.mempool_generation, generation);
    service.close();
}

#[tokio::test]
async fn transaction_that_disappears_before_fetch_is_skipped() {
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(100, 0xAB));
    source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
    source.lock().phantom.insert(txid(2));
    nfs.set(epoch(1, 100, 0xAB));

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    let snapshot = wait_for(&subscriber, is_live).await;
    assert_eq!(snapshot.tx_count, 1);
    assert!(snapshot.by_txid.contains_key(&txid(1)));
    assert!(!snapshot.by_txid.contains_key(&txid(2)));
    service.close();
}

#[tokio::test]
async fn entry_time_is_propagated() {
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(100, 0xAB));
    source.set_mempool(vec![MockTx {
        txid: txid(1),
        entry_height: 100,
        entry_time: Some(1_700_000_000),
        bytes: vec![1],
    }]);
    nfs.set(epoch(1, 100, 0xAB));

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    let snapshot = wait_for(&subscriber, is_live).await;
    assert_eq!(snapshot.by_txid[&txid(1)].entry_time, Some(1_700_000_000));
    service.close();
}

// ---- events ------------------------------------------------------------

#[tokio::test]
async fn events_added_removed_and_live_are_emitted() {
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    let mut events = subscriber.subscribe_events();

    // Add a transaction.
    source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);
    let mut saw_added = false;
    let mut saw_live = false;
    let mut last_sequence = 0u64;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(event)) => match event.as_ref() {
                MempoolEvent::Added { sequence, .. } => {
                    saw_added = true;
                    assert!(*sequence >= last_sequence);
                    last_sequence = *sequence;
                }
                MempoolEvent::Live { sequence, .. } => {
                    saw_live = true;
                    assert!(*sequence >= last_sequence);
                    last_sequence = *sequence;
                    if saw_added {
                        break;
                    }
                }
                _ => {}
            },
            _ => break,
        }
    }
    assert!(saw_added, "expected an Added event");
    assert!(saw_live, "expected a Live event");

    // Remove it.
    source.set_mempool(vec![mtx(1, 100)]);
    let mut saw_removed = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(event)) => {
                if let MempoolEvent::Removed { txid: removed, .. } = event.as_ref() {
                    assert_eq!(*removed, txid(2));
                    saw_removed = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(saw_removed, "expected a Removed event");
    service.close();
}

#[tokio::test]
async fn close_publishes_closing_snapshot_and_event() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    let mut events = subscriber.subscribe_events();
    service.close();

    let snapshot = wait_for(&subscriber, |s| matches!(s.mode, MempoolMode::Closing)).await;
    assert!(matches!(snapshot.mode, MempoolMode::Closing));

    let mut saw_closing = false;
    for _ in 0..20 {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.as_ref(), MempoolEvent::Closing { .. }) {
                    saw_closing = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(saw_closing, "expected a Closing event");
}

// ---- capacity ----------------------------------------------------------

#[tokio::test]
async fn capacity_overrun_from_empty_marks_incomplete() {
    let config = fast_config();
    config.set_max_cost_bytes(1); // any tx (min cost 10_000) breaches it
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], config);

    let snapshot = wait_for(&subscriber, |s| {
        s.completeness == MempoolCompleteness::IncompleteCapacityLimited
    })
    .await;
    assert!(!is_live(&snapshot));
    assert_ne!(snapshot.completeness, MempoolCompleteness::Complete);
    service.close();
}

#[tokio::test]
async fn capacity_overrun_while_live_freezes_preserving_prior() {
    let config = fast_config();
    // Room for one min-cost (10_000) tx but not two.
    config.set_max_cost_bytes(15_000);
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], config);
    wait_for(&subscriber, is_live).await;

    // A second tx would breach the bound.
    source.set_mempool(vec![mtx(1, 100), mtx(2, 100)]);

    let snapshot = wait_for(&subscriber, |s| {
        s.completeness == MempoolCompleteness::IncompleteCapacityLimited
    })
    .await;
    // Prior set preserved; the over-cap addition is not applied.
    assert_eq!(snapshot.tx_count, 1);
    assert!(snapshot.by_txid.contains_key(&txid(1)));
    assert_eq!(
        freeze_reason(&snapshot),
        Some(FreezeReason::CapacityLimited)
    );
    service.close();
}

#[tokio::test]
async fn max_cost_bytes_is_runtime_adjustable_via_subscriber() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    let original = subscriber.max_cost_bytes();
    subscriber.set_max_cost_bytes(42);
    assert_eq!(subscriber.max_cost_bytes(), 42);
    assert_ne!(original, 42);
    service.close();
}

// ---- frozen serving, metrics, status ----------------------------------

#[tokio::test]
async fn frozen_snapshot_is_readable() {
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(9, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    source.set_tip(block_ref(101, 0xCD));
    wait_for(&subscriber, is_frozen).await;

    assert!(subscriber.contains_txid(&txid(9)));
    assert!(subscriber.get_transaction(&txid(9)).is_some());
    assert_eq!(subscriber.get_txids().len(), 1);
    service.close();
}

#[tokio::test]
async fn get_mempool_info_reports_snapshot_metrics() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100), mtx(2, 100)], fast_config());
    wait_for(&subscriber, |s| s.tx_count == 2).await;

    let info = subscriber.get_mempool_info();
    assert_eq!(info.size, 2);
    // Two 1-byte txs; usage is the ZIP-401 cost total (>= 2 * 10_000 floor).
    assert_eq!(info.bytes, 2);
    assert!(info.usage >= 20_000);
    service.close();
}

#[tokio::test]
async fn status_reflects_lifecycle() {
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;
    assert_eq!(subscriber.status(), zaino_common::status::StatusType::Ready);

    source.set_tip(block_ref(101, 0xCD));
    wait_for(&subscriber, is_frozen).await;
    assert_eq!(
        subscriber.status(),
        zaino_common::status::StatusType::Syncing
    );
    service.close();
}

#[tokio::test]
async fn subscribers_share_entry_arcs() {
    let (service, subscriber_a, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(9, 100)], fast_config());
    let subscriber_b = service.subscriber();
    wait_for(&subscriber_a, is_live).await;
    wait_for(&subscriber_b, is_live).await;

    let entry_a = subscriber_a.get_transaction(&txid(9)).unwrap();
    let entry_b = subscriber_b.get_transaction(&txid(9)).unwrap();
    assert!(Arc::ptr_eq(&entry_a, &entry_b));
    service.close();
}

// ---- streaming ---------------------------------------------------------

#[tokio::test]
async fn stream_stays_open_on_freeze_then_closes_when_tips_reagree() {
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(100, 0xAB));
    source.set_mempool(vec![MockTx {
        txid: txid(1),
        entry_height: 100,
        entry_time: None,
        bytes: vec![0xAA, 0xBB],
    }]);
    nfs.set(epoch(1, 100, 0xAB));
    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();
    wait_for(&subscriber, is_live).await;

    let stream = subscriber
        .stream_raw_transactions(None)
        .expect("stream should open while live");
    futures::pin_mut!(stream);

    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream should yield the initial entry in time");
    assert_eq!(first, Some(vec![0xAA, 0xBB]));

    // The validator tip advances while NS lags: the mempool freezes, but the
    // stream must stay open (the tips have not re-agreed at the new tip).
    source.set_tip(block_ref(101, 0xCD));
    wait_for(&subscriber, is_frozen).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .is_err(),
        "stream must stay open while frozen (tips not yet re-agreed)"
    );

    // NS catches up so V and NS re-agree at the new tip; the mempool goes live at
    // the new epoch and the stream closes so the caller re-syncs.
    nfs.set(epoch(2, 101, 0xCD));
    loop {
        match tokio::time::timeout(Duration::from_secs(3), stream.next()).await {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => panic!("stream did not close after tips re-agreed at the new tip"),
        }
    }
    service.close();
}

#[tokio::test]
async fn stream_rejects_mismatched_expected_epoch() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    assert!(subscriber
        .stream_raw_transactions(Some(epoch(999, 100, 0xAB)))
        .is_none());
    assert!(subscriber
        .stream_raw_transactions(Some(epoch(1, 100, 0xAB)))
        .is_some());
    service.close();
}

// ---- exclude filter ----------------------------------------------------

#[tokio::test]
async fn exclude_list_bounds_are_enforced() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
    wait_for(&subscriber, is_live).await;

    assert!(subscriber
        .validate_exclude_suffixes(&vec![vec![0u8; 8]; 1025])
        .is_err()); // over count cap
    assert!(subscriber
        .validate_exclude_suffixes(&[vec![0u8; 3]])
        .is_err()); // too short
    assert!(subscriber
        .validate_exclude_suffixes(&[vec![0u8; 33]])
        .is_err()); // too long
    assert!(subscriber
        .validate_exclude_suffixes(&[vec![0u8; 8]])
        .is_ok());
    service.close();
}

#[tokio::test]
async fn unique_exclude_suffix_filters_one_ambiguous_filters_none() {
    // txid(1) is unique; the other two share their trailing four 0x22 bytes.
    let mut shared_a = [0u8; 32];
    let mut shared_b = [0x99u8; 32];
    for i in 28..32 {
        shared_a[i] = 0x22;
        shared_b[i] = 0x22;
    }
    let make = |bytes: [u8; 32]| MockTx {
        txid: TxHash(bytes),
        entry_height: 100,
        entry_time: None,
        bytes: vec![1],
    };

    let (service, subscriber, _source, _nfs) = spawn_agreeing(
        100,
        0xAB,
        1,
        vec![make([1u8; 32]), make(shared_a), make(shared_b)],
        fast_config(),
    );
    wait_for(&subscriber, |s| s.tx_count == 3).await;

    let unique = subscriber
        .validate_exclude_suffixes(&[txid(1).0.to_vec()])
        .unwrap();
    let remaining = subscriber.get_filtered_entries(&unique);
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().all(|entry| entry.txid != txid(1)));

    let ambiguous = subscriber
        .validate_exclude_suffixes(&[vec![0x22u8; 4]])
        .unwrap();
    assert_eq!(subscriber.get_filtered_entries(&ambiguous).len(), 3);
    service.close();
}

// ---- validator-only mode ----------------------------------------------

#[tokio::test]
async fn validator_only_tracks_validator_and_freezes_on_tip_change() {
    let source = MockSource::new();
    source.set_tip(block_ref(100, 0xAA));
    source.set_mempool(vec![mtx(1, 100)]);

    let service = MempoolService::spawn_validator_only(
        source.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    // Reaches Live tracking the validator alone (no NFS).
    let snapshot = wait_for(&subscriber, is_live).await;
    assert_eq!(snapshot.tx_count, 1);
    let first_epoch = snapshot.valid_for.unwrap();

    // A validator-tip change re-reconciles at the new tip (the synthesized epoch
    // follows the validator, so any freeze is transient) — the new epoch differs.
    source.set_tip(block_ref(101, 0xBB));
    source.set_mempool(vec![mtx(1, 100), mtx(2, 101)]);
    let snapshot = wait_for(&subscriber, |s| {
        is_live(s) && s.tx_count == 2 && s.valid_for != Some(first_epoch)
    })
    .await;
    assert!(snapshot.by_txid.contains_key(&txid(2)));
    service.close();
}

// ---- high-throughput / concurrency ------------------------------------

mod concurrency {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn large_mempool_reaches_live_fetching_each_once() {
        let count = 5_000u32;
        let mempool: Vec<MockTx> = (0..count)
            .map(|n| MockTx {
                txid: txid_n(n),
                entry_height: 100,
                entry_time: None,
                bytes: vec![(n % 251) as u8],
            })
            .collect();
        let (service, subscriber, source, _nfs) =
            spawn_agreeing(100, 0xAB, 1, mempool, fast_config());

        let snapshot = wait_for(&subscriber, |s| is_live(s) && s.tx_count == count as usize).await;
        assert_eq!(snapshot.tx_count, count as usize);
        // Every transaction was fetched exactly once (O(N), no re-fetch).
        for n in 0..count {
            assert_eq!(source.raw_fetch_count(&txid_n(n)), 1);
        }
        service.close();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn many_readers_never_see_torn_state() {
        // The mempool always holds one of two known sets; readers must only ever
        // observe a valid set, never a partial one.
        let set_a: Vec<MockTx> = (0..100).map(|n| mtx(n as u8, 100)).collect();
        let set_b: Vec<MockTx> = (100..200).map(|n| mtx(n as u8, 100)).collect();
        let valid_a: HashSet<TxHash> = set_a.iter().map(|t| t.txid).collect();
        let valid_b: HashSet<TxHash> = set_b.iter().map(|t| t.txid).collect();

        let (service, subscriber, source, _nfs) =
            spawn_agreeing(100, 0xAB, 1, set_a.clone(), fast_config());
        wait_for(&subscriber, |s| s.tx_count == 100).await;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Writer: flip between the two sets.
        let flip_source = source.clone();
        let flip_stop = Arc::clone(&stop);
        let flipper = tokio::spawn(async move {
            let mut toggle = false;
            while !flip_stop.load(std::sync::atomic::Ordering::Relaxed) {
                flip_source.set_mempool(if toggle { set_b.clone() } else { set_a.clone() });
                toggle = !toggle;
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
        });

        // Readers: assert every non-empty snapshot is exactly one of the sets.
        let mut readers = Vec::new();
        for _ in 0..50 {
            let reader = subscriber.clone();
            let valid_a = valid_a.clone();
            let valid_b = valid_b.clone();
            let reader_stop = Arc::clone(&stop);
            readers.push(tokio::spawn(async move {
                while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let ids: HashSet<TxHash> = reader.get_txids().iter().copied().collect();
                    if !ids.is_empty() {
                        assert!(
                            ids == valid_a || ids == valid_b,
                            "reader observed a torn set of size {}",
                            ids.len()
                        );
                    }
                    // Exercise other read paths for races/panics.
                    let _ = reader.get_mempool_info();
                    let _ = reader.snapshot();
                    tokio::task::yield_now().await;
                }
            }));
        }

        tokio::time::sleep(Duration::from_millis(300)).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        flipper.await.unwrap();
        for reader in readers {
            reader.await.unwrap();
        }
        service.close();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn many_stream_consumers_all_close_when_tips_reagree() {
        let (service, subscriber, source, nfs) =
            spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, is_live).await;

        let mut consumers = Vec::new();
        for _ in 0..20 {
            let stream = subscriber
                .stream_raw_transactions(None)
                .expect("stream should open while live");
            consumers.push(tokio::spawn(async move {
                futures::pin_mut!(stream);
                // Drain until the stream closes (new epoch) or times out.
                loop {
                    match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                        Ok(Some(_)) => continue,
                        Ok(None) => return true, // closed
                        Err(_) => return false,  // hung
                    }
                }
            }));
        }

        // Advance both tips so V and NS re-agree at a new tip: every stream should
        // close (the mempool goes live at the new epoch).
        source.set_tip(block_ref(101, 0xCD));
        nfs.set(epoch(2, 101, 0xCD));

        for consumer in consumers {
            assert!(
                consumer.await.unwrap(),
                "a stream failed to close when the tips re-agreed at the new tip"
            );
        }
        service.close();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn slow_stream_consumer_is_dropped_on_lag() {
        let mut config = fast_config();
        config.event_buffer_len = 4; // tiny buffer to force lag
        let (service, subscriber, source, _nfs) =
            spawn_agreeing(100, 0xAB, 1, vec![mtx(0, 100)], config);
        wait_for(&subscriber, is_live).await;

        // Open a stream but don't poll it while many updates flow past.
        let slow = subscriber
            .stream_raw_transactions(None)
            .expect("stream should open while live");

        // Generate many published deltas (well beyond the 4-event buffer).
        for n in 1..40u8 {
            let set: Vec<MockTx> = (0..=n).map(|b| mtx(b, 100)).collect();
            source.set_mempool(set);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The lagging stream terminates rather than hanging or delivering all.
        futures::pin_mut!(slow);
        let mut delivered = 0usize;
        loop {
            match tokio::time::timeout(Duration::from_secs(2), slow.next()).await {
                Ok(Some(_)) => {
                    delivered += 1;
                    assert!(delivered < 1_000, "lagging stream did not close");
                }
                Ok(None) => break, // closed on lag — expected
                Err(_) => panic!("lagging stream hung instead of closing"),
            }
        }
        service.close();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tip_flapping_converges_to_live() {
        let (service, subscriber, source, nfs) =
            spawn_agreeing(100, 0xAB, 1, vec![mtx(1, 100)], fast_config());
        wait_for(&subscriber, is_live).await;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Oscillate the validator tip (diverging from NS) under concurrent reads.
        let flap_source = source.clone();
        let flap_stop = Arc::clone(&stop);
        let flapper = tokio::spawn(async move {
            let mut toggle = false;
            while !flap_stop.load(std::sync::atomic::Ordering::Relaxed) {
                flap_source.set_tip(if toggle {
                    block_ref(100, 0xCD)
                } else {
                    block_ref(100, 0xAB)
                });
                toggle = !toggle;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });

        let reader = subscriber.clone();
        let read_stop = Arc::clone(&stop);
        let reader_task = tokio::spawn(async move {
            while !read_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = reader.snapshot();
                let _ = reader.get_txids();
                tokio::task::yield_now().await;
            }
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        flapper.await.unwrap();
        reader_task.await.unwrap();

        // Settle the tips in agreement; the service must converge back to Live.
        source.set_tip(block_ref(100, 0xAB));
        nfs.set(epoch(2, 100, 0xAB));
        let snapshot = wait_for(&subscriber, is_live).await;
        assert!(snapshot.by_txid.contains_key(&txid(1)));
        service.close();
    }
}
