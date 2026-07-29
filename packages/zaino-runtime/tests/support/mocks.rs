//! Shared mock components for the runtime's integration tests.
//!
//! Each mock records — into a shared [`Calls`] recorder — which tier it was
//! actually asked, so a test can assert not just the *answer* but *who
//! answered* (and, crucially, who was **not** consulted). Included via
//! `#[path = "support/mocks.rs"] mod mocks;` from each test binary (a
//! subdirectory file, so cargo doesn't compile it as its own test target).
//!
//! Each binary uses a different subset of these helpers, so unused-per-binary
//! items are expected.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt};

use zaino_core::{
    AddressBalance, Block, BlockHash, BlockId, CompactBlock, ForkPoint, Height, Locator, Outpoint,
    SpendStatus, TipEvent, Transaction, TransactionHash, TransactionLocation, TransparentAddress,
    Treestate, Utxo,
};
use zaino_fs::error::{AddressReadError, BuildError, FreezeError, HeightReadError, LookupError};
use zaino_fs::{AddressIndex, FinalisedSpine, FrozenBlock, SpendIndex, TxLocationIndex};
use zaino_nfs::{
    FollowError, FrozenOut, NfsAddressFacts, NfsSpendFacts, NfsSpine, NfsView, NonFinalisedState,
};
use zaino_runtime::{PassthroughError, PassthroughSource, Runtime, RuntimeBuilder, RuntimeConfig};

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
    /// Finalised unspent outpoints returned by `address_unspent` (default empty).
    pub finalised_utxos: Vec<Utxo>,
}

impl FinalisedSpine for MockFs {
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

impl TxLocationIndex for MockFs {
    async fn tx_location(
        &self,
        _txid: TransactionHash,
    ) -> Result<Option<TransactionLocation>, LookupError> {
        Ok(None)
    }
}

impl SpendIndex for MockFs {
    async fn spend_status(&self, _outpoint: Outpoint) -> Result<SpendStatus, LookupError> {
        Ok(SpendStatus::NoSuchOutput)
    }
}

impl AddressIndex for MockFs {
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
        Ok(self.finalised_utxos.clone())
    }
}

// --- mock NfsSnapshot (records compact_block / address; infallible reads) ---

#[derive(Clone)]
pub struct MockNfsSnap {
    pub tip: BlockId,
    pub range: (Height, Height),
    pub calls: Calls,
    /// Recent unspent outpoints returned by `address_unspent` (default empty).
    pub recent_utxos: Vec<Utxo>,
    /// Outpoints the window reports as spent (default empty).
    pub recent_spends: Vec<Outpoint>,
}

impl NfsSpine for MockNfsSnap {
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
    fn fork_point(&self, _locator: Locator) -> Option<ForkPoint> {
        None
    }
    fn chain_tips(&self) -> Vec<BlockId> {
        Vec::new()
    }
}

impl NfsSpendFacts for MockNfsSnap {
    fn spend_status(&self, outpoint: Outpoint) -> SpendStatus {
        if self.recent_spends.contains(&outpoint) {
            return SpendStatus::Spent {
                by: TransactionHash::from([0xFF; 32]),
            };
        }
        SpendStatus::NoSuchOutput
    }
}

impl NfsAddressFacts for MockNfsSnap {
    fn address_unspent(&self, _addr: &TransparentAddress) -> Vec<Utxo> {
        self.calls.record("addr-nfs".to_string());
        self.recent_utxos.clone()
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
// Implements the domain-shaped passthrough port the runtime reads through,
// recording which query was made. Returns `None` for "the validator has no such
// block/tx". It deliberately implements no synthetic-capability method, so the
// type system forbids wiring an address-history fallback to it.

pub struct MockSource {
    pub calls: Calls,
}

impl PassthroughSource for MockSource {
    async fn block_by_hash(&self, _hash: BlockHash) -> Result<Option<Block>, PassthroughError> {
        self.calls.record("source:block".to_string());
        Ok(None)
    }
    async fn transaction(
        &self,
        _txid: TransactionHash,
    ) -> Result<Option<Transaction>, PassthroughError> {
        self.calls.record("source:tx".to_string());
        Ok(None)
    }
}

/// The wired-up runtime type these mocks produce.
pub type MockRuntime = Runtime<MockFs, MockNfs, MockSource>;

/// Assemble a full-deployment runtime over the mocks (serves the address merge).
/// The axes that drive routing are explicit: the finalised `watermark`, whether
/// the NFS window is `nfs_ready` or still syncing, and whether `passthrough_enabled`.
pub async fn build_runtime(
    calls: &Calls,
    watermark: u32,
    nfs_ready: bool,
    passthrough_enabled: bool,
) -> MockRuntime {
    assemble_runtime(calls, watermark, nfs_ready, passthrough_enabled, true).await
}

/// Like [`build_runtime`], but `serve_address` controls whether the deployment
/// opts into the (type-gated) address-history merge. `false` models a minimal
/// deployment that didn't build the index.
pub async fn assemble_runtime(
    calls: &Calls,
    watermark: u32,
    nfs_ready: bool,
    passthrough_enabled: bool,
    serve_address: bool,
) -> MockRuntime {
    let fs = MockFs {
        watermark: h(watermark),
        calls: calls.clone(),
        finalised_utxos: Vec::new(),
    };
    let nfs = MockNfs {
        ready: nfs_ready,
        snap: MockNfsSnap {
            tip: block_id(150, 0xAA),
            range: (h(watermark + 1), h(150)),
            calls: calls.clone(),
            recent_utxos: Vec::new(),
            recent_spends: Vec::new(),
        },
        finalised: h(watermark),
    };
    let source = MockSource {
        calls: calls.clone(),
    };
    let assembler = RuntimeBuilder::new()
        .config(RuntimeConfig {
            passthrough_enabled,
        })
        .assemble(fs, nfs, source);
    let assembler = if serve_address {
        assembler.serving_address_history()
    } else {
        assembler
    };
    assembler.finish().await.expect("assemble")
}
