//! Smoke tests for the mempool service using in-crate mock ports.
//!
//! The full tip-coherence / update matrix lands in Stage 5; these exercise the
//! happy path (agreement -> live) and the core freeze invariant (disagreement ->
//! frozen, no transactions) so the state machine is wired up correctly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zebra_chain::{
    block::{Hash as BlockHash, Height},
    transaction::{Hash as TxHash, SerializedTransaction},
};

use crate::config::MempoolConfig;
use crate::ports::{BlockRef, MempoolSource, MempoolTxMeta, NfsEpochObserver, NonFinalizedEpoch};
use crate::snapshot::{MempoolMode, MempoolSnapshot};
use crate::subscriber::MempoolSubscriber;
use crate::{MempoolError, MempoolService};

#[derive(Default)]
struct MockSourceState {
    tip: Option<BlockRef>,
    /// `(txid, entry_height, raw_bytes)` currently in the source mempool.
    mempool: Vec<(TxHash, u32, Vec<u8>)>,
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
}

impl MempoolSource for MockSource {
    async fn get_mempool_metadata(&self) -> Result<Option<Vec<MempoolTxMeta>>, MempoolError> {
        let state = self.state.lock().expect("mock source poisoned");
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
        let state = self.state.lock().expect("mock source poisoned");
        Ok(state
            .mempool
            .iter()
            .find(|(candidate, _, _)| *candidate == txid)
            .map(|(_, _, bytes)| SerializedTransaction::from(bytes.clone())))
    }

    async fn get_mempool_source_tip(&self) -> Result<Option<BlockRef>, MempoolError> {
        Ok(self.state.lock().expect("mock source poisoned").tip)
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
}

impl NfsEpochObserver for MockNfs {
    fn current_epoch(&self) -> Option<NonFinalizedEpoch> {
        *self.epoch.lock().expect("mock nfs poisoned")
    }
}

fn block_ref(height: u32, hash_byte: u8) -> BlockRef {
    BlockRef {
        height: Height(height),
        hash: BlockHash([hash_byte; 32]),
    }
}

fn fast_config() -> MempoolConfig {
    let mut config = MempoolConfig::default();
    config.poll_interval = Duration::from_millis(5);
    config
}

async fn wait_for(
    subscriber: &MempoolSubscriber,
    predicate: impl Fn(&MempoolSnapshot) -> bool,
) -> Arc<MempoolSnapshot> {
    for _ in 0..400 {
        let snapshot = subscriber.snapshot();
        if predicate(&snapshot) {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("mempool snapshot never satisfied the predicate");
}

// current-thread: the mock source/nfs ports never block; awaiting the polling
// sleep is enough to let the spawned service task make progress.
#[tokio::test]
async fn agreement_publishes_a_live_snapshot() {
    let source = MockSource::new();
    let nfs = MockNfs::new();

    let tip = block_ref(100, 0xAB);
    {
        let mut state = source.state.lock().unwrap();
        state.tip = Some(tip);
        state.mempool = vec![
            (TxHash([1; 32]), 100, vec![0xDE, 0xAD]),
            (TxHash([2; 32]), 100, vec![0xBE, 0xEF, 0x01]),
        ];
    }
    let epoch = NonFinalizedEpoch {
        generation: 7,
        best_tip: tip,
    };
    *nfs.epoch.lock().unwrap() = Some(epoch);

    let service = MempoolService::spawn(source, nfs, fast_config(), CancellationToken::new());
    let subscriber = service.subscriber();

    let snapshot = wait_for(&subscriber, |snapshot| {
        matches!(snapshot.mode, MempoolMode::Live { .. })
    })
    .await;

    assert!(snapshot.is_live_for(epoch));
    assert_eq!(snapshot.tx_count, 2);
    assert!(snapshot.by_txid.contains_key(&TxHash([1; 32])));
    assert!(snapshot.by_txid.contains_key(&TxHash([2; 32])));
    // entry_height was sourced from the metadata, not derived.
    assert_eq!(snapshot.by_txid[&TxHash([1; 32])].entry_height, Height(100));

    service.close();
}

#[tokio::test]
async fn disagreeing_tips_freeze_with_no_transactions() {
    let source = MockSource::new();
    let nfs = MockNfs::new();

    // V and NS point at different tip hashes at the same height.
    {
        let mut state = source.state.lock().unwrap();
        state.tip = Some(block_ref(100, 0xAA));
        state.mempool = vec![(TxHash([1; 32]), 100, vec![0x01])];
    }
    *nfs.epoch.lock().unwrap() = Some(NonFinalizedEpoch {
        generation: 1,
        best_tip: block_ref(100, 0xBB),
    });

    let service = MempoolService::spawn(source, nfs, fast_config(), CancellationToken::new());
    let subscriber = service.subscriber();

    let snapshot = wait_for(&subscriber, |snapshot| {
        matches!(snapshot.mode, MempoolMode::Frozen { .. })
    })
    .await;

    // Frozen with no coherent set: transactions must never be applied while the
    // tips disagree.
    assert!(matches!(snapshot.mode, MempoolMode::Frozen { .. }));
    assert_eq!(snapshot.tx_count, 0);
    assert_eq!(snapshot.valid_for, None);

    service.close();
}
