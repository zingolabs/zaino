//! The mempool service test matrix, driven by in-crate mock ports.
//!
//! These cover the freeze/thaw tip-coherence rules, incremental update
//! behaviour, capacity bounds, frozen serving, and stream/subscriber semantics
//! required by the mempool spec. Epoch-gated *combined* ChainIndex reads
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
use crate::ports::{BlockRef, MempoolSource, MempoolTxMeta, NfsEpochObserver, NonFinalizedEpoch};
use crate::snapshot::{MempoolCompleteness, MempoolMode, MempoolSnapshot};
use crate::subscriber::MempoolSubscriber;
use crate::{MempoolError, MempoolService};

// ---- mock ports --------------------------------------------------------

#[derive(Default)]
struct MockSourceState {
    tip: Option<BlockRef>,
    /// `(txid, entry_height, raw_bytes)` currently listed in the source mempool.
    mempool: Vec<(TxHash, u32, Vec<u8>)>,
    /// Txids that are listed by metadata but whose raw fetch returns `None`
    /// (they disappeared between listing and fetch).
    phantom: HashSet<TxHash>,
    /// Number of raw-transaction fetches issued per txid.
    raw_fetch_counts: HashMap<TxHash, usize>,
    /// If set, the next raw fetch advances the tip to this value (once),
    /// simulating a chain-tip change mid-update.
    advance_tip_on_next_fetch: Option<BlockRef>,
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

    fn set_mempool(&self, mempool: Vec<(TxHash, u32, Vec<u8>)>) {
        self.lock().mempool = mempool;
    }

    fn set_tip(&self, tip: BlockRef) {
        self.lock().tip = Some(tip);
    }

    fn raw_fetch_count(&self, txid: &TxHash) -> usize {
        self.lock().raw_fetch_counts.get(txid).copied().unwrap_or(0)
    }
}

impl MempoolSource for MockSource {
    async fn get_mempool_metadata(&self) -> Result<Option<Vec<MempoolTxMeta>>, MempoolError> {
        let state = self.lock();
        Ok(Some(
            state
                .mempool
                .iter()
                .map(|(txid, height, _)| MempoolTxMeta {
                    txid: *txid,
                    entry_height: Height(*height),
                    entry_time: None,
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
            .find(|(candidate, _, _)| *candidate == txid)
            .map(|(_, _, bytes)| SerializedTransaction::from(bytes.clone())))
    }

    async fn get_mempool_source_tip(&self) -> Result<Option<BlockRef>, MempoolError> {
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

/// Spawn a service whose validator tip and non-finalized epoch already agree at
/// `(height, hash_byte)`, with `mempool` listed. Returns the service, a
/// subscriber, and the mock handles for further manipulation.
fn spawn_agreeing(
    height: u32,
    hash_byte: u8,
    generation: u64,
    mempool: Vec<(TxHash, u32, Vec<u8>)>,
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
    for _ in 0..600 {
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

// ---- tip coherence -----------------------------------------------------

#[tokio::test]
async fn agreement_publishes_a_live_snapshot() {
    let (service, subscriber, _source, _nfs) = spawn_agreeing(
        100,
        0xAB,
        7,
        vec![
            (txid(1), 100, vec![0xDE, 0xAD]),
            (txid(2), 100, vec![1, 2, 3]),
        ],
        fast_config(),
    );

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
        spawn_agreeing(100, 0xAB, 1, vec![(txid(1), 100, vec![9])], fast_config());
    wait_for(&subscriber, is_live).await;

    // V advances while NS stays: the tips diverge.
    source.set_tip(block_ref(101, 0xCD));

    let snapshot = wait_for(&subscriber, is_frozen).await;
    // The last coherent transaction set stays readable while frozen.
    assert_eq!(snapshot.tx_count, 1);
    assert!(snapshot.by_txid.contains_key(&txid(1)));
    assert_eq!(snapshot.valid_for, Some(epoch(1, 100, 0xAB)));

    service.close();
}

#[tokio::test]
async fn nonfinalized_tip_change_freezes() {
    let (service, subscriber, _source, nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![(txid(1), 100, vec![9])], fast_config());
    wait_for(&subscriber, is_live).await;

    // NS advances while V stays: the tips diverge.
    nfs.set(epoch(2, 101, 0xCD));

    let snapshot = wait_for(&subscriber, is_frozen).await;
    assert_eq!(snapshot.tx_count, 1); // preserved
    service.close();
}

#[tokio::test]
async fn agreement_after_divergence_thaws_to_live() {
    // Start diverged: V at hash 0xAA, NS at hash 0xBB.
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(100, 0xAA));
    source.set_mempool(vec![(txid(1), 100, vec![7])]);
    nfs.set(epoch(1, 100, 0xBB));

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    wait_for(&subscriber, is_frozen).await;

    // Bring the tips into agreement.
    nfs.set(epoch(2, 100, 0xAA));

    let snapshot = wait_for(&subscriber, is_live).await;
    assert_eq!(snapshot.tx_count, 1);
    assert!(snapshot.is_live_for(epoch(2, 100, 0xAA)));
    service.close();
}

#[tokio::test]
async fn missing_nonfinalized_state_stays_not_ready() {
    // V is available but NS never exists.
    let source = MockSource::new();
    let nfs = MockNfs::new();
    source.set_tip(block_ref(100, 0xAA));
    source.set_mempool(vec![(txid(1), 100, vec![7])]);

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    // Give the loop time to run several ticks; with no NS it must never apply
    // transactions.
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
    source.set_mempool(vec![(txid(1), 100, vec![7])]);
    nfs.set(epoch(1, 100, 0xAA));
    // The first raw fetch advances V's tip, so the post-fetch coherence guard
    // fails and the fetched transaction must be discarded (not published).
    source.lock().advance_tip_on_next_fetch = Some(block_ref(101, 0xCC));

    let service = MempoolService::spawn(
        source.clone(),
        nfs.clone(),
        fast_config(),
        CancellationToken::new(),
    );
    let subscriber = service.subscriber();

    // NS never follows to 0xCC, so V and NS stay diverged and the service never
    // publishes the raced transaction.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let snapshot = subscriber.snapshot();
    assert!(!snapshot.is_live_for(epoch(1, 100, 0xAA)));
    assert_eq!(snapshot.tx_count, 0);
    service.close();
}

// ---- transaction updates ----------------------------------------------

#[tokio::test]
async fn added_transaction_is_fetched_once() {
    let (service, subscriber, source, _nfs) = spawn_agreeing(
        100,
        0xAB,
        1,
        vec![(txid(1), 100, vec![1]), (txid(2), 100, vec![2])],
        fast_config(),
    );
    wait_for(&subscriber, |s| s.tx_count == 2).await;

    // Let many further poll cycles run: existing transactions must not be
    // re-fetched.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(source.raw_fetch_count(&txid(1)), 1);
    assert_eq!(source.raw_fetch_count(&txid(2)), 1);
    service.close();
}

#[tokio::test]
async fn removed_transaction_is_dropped_when_live() {
    let (service, subscriber, source, _nfs) = spawn_agreeing(
        100,
        0xAB,
        1,
        vec![(txid(1), 100, vec![1]), (txid(2), 100, vec![2])],
        fast_config(),
    );
    wait_for(&subscriber, |s| s.tx_count == 2).await;

    // Drop txid(2) from the source mempool.
    source.set_mempool(vec![(txid(1), 100, vec![1])]);

    let snapshot = wait_for(&subscriber, |s| s.tx_count == 1).await;
    assert!(snapshot.by_txid.contains_key(&txid(1)));
    assert!(!snapshot.by_txid.contains_key(&txid(2)));
    service.close();
}

#[tokio::test]
async fn unchanged_set_does_not_republish() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![(txid(1), 100, vec![1])], fast_config());
    let live = wait_for(&subscriber, is_live).await;
    let generation = live.mempool_generation;

    // Many further ticks with an unchanged set must not publish a new snapshot.
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
    source.set_mempool(vec![(txid(1), 100, vec![1]), (txid(2), 100, vec![2])]);
    // txid(2) is listed by metadata but its raw fetch returns None.
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
    // The update succeeds with the surviving tx; the disappeared one is skipped,
    // not a failure.
    assert_eq!(snapshot.tx_count, 1);
    assert!(snapshot.by_txid.contains_key(&txid(1)));
    assert!(!snapshot.by_txid.contains_key(&txid(2)));
    service.close();
}

// ---- capacity ----------------------------------------------------------

#[tokio::test]
async fn capacity_overrun_marks_incomplete_never_complete() {
    let config = fast_config();
    // Any single transaction (min cost 10_000) breaches a 1-byte bound.
    config.set_max_cost_bytes(1);
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![(txid(1), 100, vec![1])], config);

    let snapshot = wait_for(&subscriber, |s| {
        s.completeness == MempoolCompleteness::IncompleteCapacityLimited
    })
    .await;
    // Never claims Complete, and stays within the bound (nothing published).
    assert!(!is_live(&snapshot));
    assert_ne!(snapshot.completeness, MempoolCompleteness::Complete);
    service.close();
}

// ---- frozen serving & sharing -----------------------------------------

#[tokio::test]
async fn frozen_snapshot_is_readable() {
    let (service, subscriber, source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![(txid(1), 100, vec![9])], fast_config());
    wait_for(&subscriber, is_live).await;

    source.set_tip(block_ref(101, 0xCD)); // diverge -> freeze
    wait_for(&subscriber, is_frozen).await;

    // Reads still serve the last coherent set while frozen.
    assert!(subscriber.contains_txid(&txid(1)));
    assert!(subscriber.get_transaction(&txid(1)).is_some());
    assert_eq!(subscriber.get_txids().len(), 1);
    service.close();
}

#[tokio::test]
async fn subscribers_share_entry_arcs() {
    let (service, subscriber_a, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![(txid(1), 100, vec![9])], fast_config());
    let subscriber_b = service.subscriber();
    wait_for(&subscriber_a, is_live).await;
    wait_for(&subscriber_b, is_live).await;

    let entry_a = subscriber_a.get_transaction(&txid(1)).unwrap();
    let entry_b = subscriber_b.get_transaction(&txid(1)).unwrap();
    // One shared entry, not a per-subscriber byte clone.
    assert!(Arc::ptr_eq(&entry_a, &entry_b));
    service.close();
}

// ---- streaming ---------------------------------------------------------

#[tokio::test]
async fn stream_yields_initial_then_closes_on_freeze() {
    let (service, subscriber, source, _nfs) = spawn_agreeing(
        100,
        0xAB,
        1,
        vec![(txid(1), 100, vec![0xAA, 0xBB])],
        fast_config(),
    );
    wait_for(&subscriber, is_live).await;

    let stream = subscriber
        .stream_raw_transactions(None)
        .expect("stream should open while live");
    futures::pin_mut!(stream);

    // The initial snapshot entry is delivered.
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream should yield the initial entry in time");
    assert_eq!(first, Some(vec![0xAA, 0xBB]));

    // Freeze: the stream must terminate rather than deliver stale data.
    source.set_tip(block_ref(101, 0xCD));
    loop {
        match tokio::time::timeout(Duration::from_secs(3), stream.next()).await {
            Ok(Some(_)) => continue,
            Ok(None) => break, // stream closed on Frozen — expected
            Err(_) => panic!("stream did not close after freeze"),
        }
    }
    service.close();
}

#[tokio::test]
async fn stream_rejects_mismatched_expected_epoch() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![(txid(1), 100, vec![9])], fast_config());
    wait_for(&subscriber, is_live).await;

    // A caller whose snapshot epoch does not match the mempool's is refused.
    assert!(subscriber
        .stream_raw_transactions(Some(epoch(999, 100, 0xAB)))
        .is_none());
    // The matching epoch opens.
    assert!(subscriber
        .stream_raw_transactions(Some(epoch(1, 100, 0xAB)))
        .is_some());
    service.close();
}

// ---- exclude-filter bounds --------------------------------------------

#[tokio::test]
async fn exclude_list_bounds_are_enforced() {
    let (service, subscriber, _source, _nfs) =
        spawn_agreeing(100, 0xAB, 1, vec![(txid(1), 100, vec![9])], fast_config());
    wait_for(&subscriber, is_live).await;

    // Over the aggregate count cap (default 1024).
    let too_many = vec![vec![0u8; 8]; 1025];
    assert!(subscriber.validate_exclude_suffixes(&too_many).is_err());

    // Below the minimum suffix length (default 4).
    assert!(subscriber
        .validate_exclude_suffixes(&[vec![0u8; 3]])
        .is_err());

    // Above the maximum suffix length (default 32).
    assert!(subscriber
        .validate_exclude_suffixes(&[vec![0u8; 33]])
        .is_err());

    // A well-formed suffix validates.
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

    let (service, subscriber, _source, _nfs) = spawn_agreeing(
        100,
        0xAB,
        1,
        vec![
            (txid(1), 100, vec![1]),
            (TxHash(shared_a), 100, vec![2]),
            (TxHash(shared_b), 100, vec![3]),
        ],
        fast_config(),
    );
    wait_for(&subscriber, |s| s.tx_count == 3).await;

    // Unique suffix (the whole txid(1)) excludes exactly that transaction.
    let unique = subscriber
        .validate_exclude_suffixes(&[txid(1).0.to_vec()])
        .unwrap();
    let remaining = subscriber.get_filtered_entries(&unique);
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().all(|entry| entry.txid != txid(1)));

    // Ambiguous suffix (shared trailing 0x22) matches two -> excludes none.
    let ambiguous = subscriber
        .validate_exclude_suffixes(&[vec![0x22u8; 4]])
        .unwrap();
    assert_eq!(subscriber.get_filtered_entries(&ambiguous).len(), 3);
    service.close();
}
