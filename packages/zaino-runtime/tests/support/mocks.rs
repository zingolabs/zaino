//! Shared mock components for the runtime's integration tests.
//!
//! Each mock records — into a shared [`Calls`] recorder — which tier it was
//! actually asked, so a test can assert not just the *answer* but *who
//! answered* (and, crucially, who was **not** consulted). Included via
//! `#[path = "support/mocks.rs"] mod mocks;` from each test binary (a
//! subdirectory file, so cargo doesn't compile it as its own test target).

use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt};

use zaino_core::{
    AddressBalance, Block, BlockHash, BlockId, CompactBlock, ForkPoint, Height, Locator, Outpoint,
    SpendStatus, TipEvent, TransactionHash, TransactionLocation, TransparentAddress, Treestate, Utxo,
};
use zaino_fs::error::{AddressReadError, BuildError, FreezeError, HeightReadError, LookupError};
use zaino_fs::{FinalisedState, FrozenBlock};
use zaino_nfs::{FollowError, FrozenOut, NfsSnapshot, NfsView, NonFinalisedState};
use zaino_runtime::{Runtime, RuntimeBuilder, RuntimeConfig};
use zaino_source::{
    GetBlockByHash, GetBlockByHashError, GetTransaction, GetTransactionError, QueryError,
    TransactionResponse,
};

/// A shared call recorder, so a test can see which tier answered.
#[derive(Clone, Default)]
pub struct Calls(Arc<Mutex<Vec<String>>>);

impl Calls {
    pub fn record(&self, s: String) {
        self.0.lock().expect("calls mutex").push(s);
    }
    pub fn log(&self) -> Vec<String> {
        self.0.lock().expect("calls mutex").clone()
    }
}

pub fn h(n: u32) -> Height {
    Height::try_from(n).expect("valid height")
}

pub fn block_id(height: u32, tag: u8) -> BlockId {
    BlockId {
        height: h(height),
        hash: BlockHash::from([tag; 32]),
    }
}

// --- mock FinalisedState (records compact_block / address; rest is minimal) ---

pub struct MockFs {
    pub watermark: Height,
    pub calls: Calls,
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
        unimplemented!("not exercised by the routing tests")
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

// --- mock NfsSnapshot (records compact_block / address; infallible reads) ---

#[derive(Clone)]
pub struct MockNfsSnap {
    pub tip: BlockId,
    pub range: (Height, Height),
    pub calls: Calls,
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

pub struct MockNfs {
    pub ready: bool,
    pub snap: MockNfsSnap,
    pub finalised: Height,
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

// --- mock validator Source (the passthrough provider) ---
//
// Implements the `zaino-source` read ports the runtime passes through to,
// recording which query was made. A `NotFound` domain answer stands in for
// "the validator has no such block/tx" (→ the runtime maps it to `None`); it
// deliberately implements no synthetic-capability port, so the type system
// forbids wiring an address-history fallback to it.

pub struct MockSource {
    pub calls: Calls,
}

impl GetBlockByHash for MockSource {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.calls.record("source:block".to_string());
        Err(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl GetTransaction for MockSource {
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.calls.record("source:tx".to_string());
        Err(QueryError::Domain(GetTransactionError::NotFound(txid)))
    }
}

/// The wired-up runtime type these mocks produce.
pub type MockRuntime = Runtime<MockFs, MockNfs, MockSource>;

/// Assemble a runtime over the mocks against a single shared recorder. The
/// axes that drive routing are explicit: the finalised `watermark`, whether the
/// NFS window is `nfs_ready` or still syncing, and whether `passthrough_enabled`.
pub async fn build_runtime(
    calls: &Calls,
    watermark: u32,
    nfs_ready: bool,
    passthrough_enabled: bool,
) -> MockRuntime {
    let fs = MockFs {
        watermark: h(watermark),
        calls: calls.clone(),
    };
    let nfs = MockNfs {
        ready: nfs_ready,
        snap: MockNfsSnap {
            tip: block_id(150, 0xAA),
            range: (h(watermark + 1), h(150)),
            calls: calls.clone(),
        },
        finalised: h(watermark),
    };
    let source = MockSource {
        calls: calls.clone(),
    };
    RuntimeBuilder::new()
        .config(RuntimeConfig {
            passthrough_enabled,
        })
        .init(fs, nfs, source)
        .await
        .expect("init")
}
