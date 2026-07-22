//! Exercises the compose seam: `Runtime` routes reads FS (`≤ watermark`) vs NFS
//! (`> watermark`), and returns `NotServiceable` for recent reads while the NFS
//! window is syncing. Component mocks record which tier they were asked, so the
//! routing is asserted directly.

use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt};

use zaino_core::{
    AddressBalance, BlockHash, BlockId, BlockRef, CompactBlock, ForkPoint, Height, Locator,
    Outpoint, SpendStatus, TipEvent, TransactionHash, TransactionLocation, TransparentAddress,
    Treestate, Utxo,
};
use zaino_fs::error::{AddressReadError, BuildError, FreezeError, HeightReadError, LookupError};
use zaino_fs::{FinalisedState, FrozenBlock};
use zaino_nfs::{FollowError, FrozenOut, NfsSnapshot, NfsView, NonFinalisedState};
use zaino_runtime::RuntimeBuilder;
use zaino_service::error::BlockReadError;
use zaino_service::{AddressRead, CompactBlockRead};

// --- a shared call recorder, so we can see which tier answered ---

#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<String>>>);

impl Calls {
    fn record(&self, s: String) {
        self.0.lock().expect("calls mutex").push(s);
    }
    fn log(&self) -> Vec<String> {
        self.0.lock().expect("calls mutex").clone()
    }
}

fn h(n: u32) -> Height {
    Height::try_from(n).expect("valid height")
}

fn block_id(height: u32, tag: u8) -> BlockId {
    BlockId {
        height: h(height),
        hash: BlockHash::from([tag; 32]),
    }
}

// --- mock FinalisedState (records compact_block; rest is minimal) ---

struct MockFs {
    watermark: Height,
    calls: Calls,
}

impl FinalisedState for MockFs {
    fn watermark(&self) -> Height {
        self.watermark
    }
    async fn compact_block(&self, height: Height) -> Result<Option<CompactBlock>, HeightReadError> {
        self.calls.record(format!("fs:{}", u32::from(height)));
        Ok(None)
    }
    async fn treestate(&self, _height: Height) -> Result<Treestate, HeightReadError> {
        unimplemented!("not exercised by the routing test")
    }
    async fn height_of(&self, _hash: BlockHash) -> Result<Option<Height>, LookupError> {
        Ok(None)
    }
    async fn tx_location(
        &self,
        _txid: TransactionHash,
    ) -> Result<Option<TransactionLocation>, LookupError> {
        Ok(None)
    }
    async fn spend_status(&self, _outpoint: Outpoint) -> Result<SpendStatus, LookupError> {
        Ok(SpendStatus::NoSuchOutput)
    }
    async fn address_balance(
        &self,
        _addr: &TransparentAddress,
    ) -> Result<AddressBalance, AddressReadError> {
        Err(AddressReadError::NotEnabled)
    }
    async fn address_unspent(
        &self,
        _addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        self.calls.record("addr-fs".to_string());
        Ok(Vec::new())
    }
    async fn bulk_build_to<S: Send + Sync>(
        &self,
        _target: Height,
        _source: &S,
    ) -> Result<(), BuildError> {
        Ok(())
    }
    async fn freeze(&self, _block: FrozenBlock) -> Result<(), FreezeError> {
        Ok(())
    }
}

// --- mock NfsSnapshot (records compact_block; infallible in-memory reads) ---

#[derive(Clone)]
struct MockNfsSnap {
    tip: BlockId,
    range: (Height, Height),
    calls: Calls,
}

impl NfsSnapshot for MockNfsSnap {
    fn tip(&self) -> BlockId {
        self.tip
    }
    fn range(&self) -> (Height, Height) {
        self.range
    }
    fn compact_block(&self, height: Height) -> Option<CompactBlock> {
        self.calls.record(format!("nfs:{}", u32::from(height)));
        None
    }
    fn height_of(&self, _hash: BlockHash) -> Option<Height> {
        None
    }
    fn spend_status(&self, _outpoint: Outpoint) -> SpendStatus {
        SpendStatus::NoSuchOutput
    }
    fn fork_point(&self, _locator: Locator) -> Option<ForkPoint> {
        None
    }
    fn address_unspent(&self, _addr: &TransparentAddress) -> Vec<Utxo> {
        self.calls.record("addr-nfs".to_string());
        Vec::new()
    }
    fn chain_tips(&self) -> Vec<BlockId> {
        Vec::new()
    }
}

// --- mock NonFinalisedState (Ready vs Syncing) ---

struct MockNfs {
    ready: bool,
    snap: MockNfsSnap,
    finalised: Height,
}

impl NonFinalisedState for MockNfs {
    type Snapshot = MockNfsSnap;
    fn snapshot(&self) -> NfsView<MockNfsSnap> {
        if self.ready {
            NfsView::Ready(self.snap.clone())
        } else {
            NfsView::Syncing {
                finalised: self.finalised,
            }
        }
    }
    fn subscribe_tip(&self) -> BoxStream<'_, TipEvent> {
        stream::empty().boxed()
    }
    fn frozen(&self) -> BoxStream<'_, FrozenOut> {
        stream::empty().boxed()
    }
    async fn follow<S: Send + Sync>(&self, _source: &S) -> Result<(), FollowError> {
        Ok(())
    }
}

#[tokio::test]
async fn routes_finalised_to_fs_and_recent_to_nfs() {
    let calls = Calls::default();
    let fs = MockFs {
        watermark: h(100),
        calls: calls.clone(),
    };
    let nfs = MockNfs {
        ready: true,
        snap: MockNfsSnap {
            tip: block_id(150, 0xAA),
            range: (h(101), h(150)),
            calls: calls.clone(),
        },
        finalised: h(100),
    };

    let runtime = RuntimeBuilder::new().init(fs, nfs, ()).await.expect("init");
    let snap = runtime.snapshot();

    // Finalised height (<= watermark) → FS.
    assert!(snap
        .compact_block(BlockRef::Height(h(50)))
        .await
        .expect("fs read ok")
        .is_none());
    // Recent height (> watermark) → NFS.
    assert!(snap
        .compact_block(BlockRef::Height(h(120)))
        .await
        .expect("nfs read ok")
        .is_none());

    assert_eq!(calls.log(), vec!["fs:50".to_string(), "nfs:120".to_string()]);
}

#[tokio::test]
async fn recent_reads_are_not_serviceable_while_nfs_syncs() {
    let calls = Calls::default();
    let fs = MockFs {
        watermark: h(100),
        calls: calls.clone(),
    };
    let nfs = MockNfs {
        ready: false, // still catching up
        snap: MockNfsSnap {
            tip: block_id(0, 0),
            range: (h(0), h(0)),
            calls: calls.clone(),
        },
        finalised: h(100),
    };

    let runtime = RuntimeBuilder::new().init(fs, nfs, ()).await.expect("init");
    let snap = runtime.snapshot();

    // Recent read while the window is syncing → NotServiceable, and NFS is never
    // consulted (no false `None`).
    let res = snap.compact_block(BlockRef::Height(h(120))).await;
    assert!(matches!(res, Err(BlockReadError::NotServiceable(_))));
    assert!(
        calls.log().iter().all(|c| !c.starts_with("nfs:")),
        "NFS must not be read while syncing, got {:?}",
        calls.log()
    );
}

/// US-1.3: an address's unspent set is a merge of both tiers, not a route —
/// both FS and NFS must be consulted.
#[tokio::test]
async fn address_unspent_merges_fs_and_nfs() {
    let calls = Calls::default();
    let fs = MockFs {
        watermark: h(100),
        calls: calls.clone(),
    };
    let nfs = MockNfs {
        ready: true,
        snap: MockNfsSnap {
            tip: block_id(150, 0xAA),
            range: (h(101), h(150)),
            calls: calls.clone(),
        },
        finalised: h(100),
    };

    let runtime = RuntimeBuilder::new().init(fs, nfs, ()).await.expect("init");
    let snap = runtime.snapshot();

    let addr = TransparentAddress::new("t1example".to_string());
    let utxos = snap.unspent_outpoints(&addr).await.expect("merge ok");
    assert!(utxos.is_empty());

    let log = calls.log();
    assert!(log.contains(&"addr-fs".to_string()), "FS not consulted: {log:?}");
    assert!(log.contains(&"addr-nfs".to_string()), "NFS not consulted: {log:?}");
}
