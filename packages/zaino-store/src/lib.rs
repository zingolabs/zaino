//! # zaino-store — EXPLORATORY STUB, NOT FOR PRODUCTION
//!
//! ⚠️ **This crate is a throwaway spike.** It exists to prove one seam: that the
//! runtime's **finalised read component** can be a thin reader over the KV
//! backend that *composes indexes on read* into the [`zaino_service`] read
//! ports — with no versioned store and no migrations. It is deliberately
//! incomplete: most read traits are unimplemented, error handling is coarse,
//! reads run synchronously (no `spawn_blocking`), and address decoding is
//! stubbed. **Do not build on it.** It will be superseded by the real FS serving
//! component (and/or converged with ChainView). The `format!`-in-error and
//! stubbed paths here are acceptable *only* because this is exploratory
//! ([[error-propagation-rule]] is relaxed for sure-throwaway code).
//!
//! What it demonstrates today:
//! - a [`StoreReader`] handle over any [`Backend`], yielding a pinned
//!   [`StoreSnapshot`];
//! - [`StoreSnapshot`] implements the [`Snapshot`] coherence marker (tip +
//!   serviceable range) — the real, working part;
//! - one compose-on-read path: [`AddressRead::tx_ids`] scans the
//!   `address_history` index via `read_receives` and projects to txids. The
//!   address-decode dependency is stubbed, so it reports not-serviceable until
//!   that lands — but the reader → index → domain wiring compiles end-to-end.
#![forbid(unsafe_code)]

use std::future::Future;
use std::sync::Arc;

use zaino_core::{
    AddressBalance, AddressDelta, BlockId, Capability, Height, HeightRange, ServiceableRange,
    TransactionId, TransparentAddress, Utxo,
};
use zaino_indexes::indexes::address_history::{self, AddrId};
use zaino_persistence::Backend;
use zaino_service::error::{AddressReadError, Transient};
use zaino_service::{AddressRead, Snapshot, TakeSnapshot};

/// EXPLORATORY: a read handle over the KV backend. Holds the backend plus the
/// coherence coordinates the writer would publish (stubbed as constructor args
/// until that wiring exists).
pub struct StoreReader<B> {
    backend: Arc<B>,
    tip: Option<BlockId>,
    finalized_tip: Height,
}

impl<B> StoreReader<B> {
    /// EXPLORATORY constructor. `tip` / `finalized_tip` stand in for the
    /// writer's published watermark until the indexer component reports it.
    pub fn new(backend: Arc<B>, tip: Option<BlockId>, finalized_tip: Height) -> Self {
        Self {
            backend,
            tip,
            finalized_tip,
        }
    }
}

impl<B: Backend + 'static> TakeSnapshot for StoreReader<B> {
    type Snapshot = StoreSnapshot<B>;

    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send {
        // Infallible in this stub: a KV reader is cheap and always available, so
        // there is no reorg-swap race to surface as `Transient`.
        let snapshot = StoreSnapshot {
            backend: self.backend.clone(),
            tip: self.tip,
            finalized_tip: self.finalized_tip,
        };
        async move { Ok(snapshot) }
    }
}

/// EXPLORATORY: an immutable pinned view. Clones share the backend via `Arc`.
pub struct StoreSnapshot<B> {
    backend: Arc<B>,
    tip: Option<BlockId>,
    finalized_tip: Height,
}

// Manual `Clone` so the bound is on `Arc<B>` (always cloneable), not `B`.
impl<B> Clone for StoreSnapshot<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            tip: self.tip,
            finalized_tip: self.finalized_tip,
        }
    }
}

impl<B: Backend + 'static> Snapshot for StoreSnapshot<B> {
    fn pinned_tip(&self) -> Option<BlockId> {
        self.tip
    }

    fn serviceable_range(&self) -> ServiceableRange {
        // Stub: with no non-finalised window wired, the view answers up to the
        // finalised tip only, so `tip == finalized_tip` when a tip is pinned.
        ServiceableRange {
            finalized_tip: self.finalized_tip,
            tip: self.tip.map_or(self.finalized_tip, |block| block.height),
        }
    }
}

impl<B: Backend + 'static> AddressRead for StoreSnapshot<B> {
    async fn balance(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        Err(not_built())
    }

    async fn unspent_outpoints(
        &self,
        _addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        Err(not_built())
    }

    async fn deltas(
        &self,
        _addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        Err(not_built())
    }

    /// The one real compose-on-read path: scan `address_history`, project to
    /// txids. EXPLORATORY: the height-window filter and de-dup are TODO (they
    /// need `BlockHeight` ↔ `Height`), and the address decode is stubbed.
    fn tx_ids(
        &self,
        addr: &TransparentAddress,
        _range: HeightRange,
    ) -> impl Future<Output = Result<Vec<TransactionId>, AddressReadError>> + Send {
        let backend = self.backend.clone();
        let decoded = decode_address(addr);
        async move {
            let addr_id = decoded?;
            // Stub: synchronous read on the async path — a real reader would
            // `spawn_blocking` around the scan.
            let reader = backend
                .reader()
                .map_err(|e| AddressReadError::Fatal(format!("open reader: {e}")))?;
            let receives = address_history::read_receives(&reader, addr_id)
                .map_err(|e| AddressReadError::Fatal(format!("read address_history: {e}")))?;
            // TODO(stub): filter to `range` and de-dup once the height types bridge.
            Ok(receives.into_iter().map(|receive| receive.txid).collect())
        }
    }
}

/// EXPLORATORY: a read whose index this stub does not build yet reports
/// not-serviceable rather than panicking.
fn not_built() -> AddressReadError {
    AddressReadError::NotServiceable(Capability::AddressHistory)
}

/// EXPLORATORY STUB. Decoding a [`TransparentAddress`] (a base58/bech32 string)
/// into the index key [`AddrId`] (`script_type` + 20-byte hash) needs the
/// address stack (zaino-address / librustzcash). Until that is wired, address
/// reads report not-serviceable rather than fabricate a key — so the seam
/// compiles and the read path is exercised the moment decoding lands.
fn decode_address(_addr: &TransparentAddress) -> Result<AddrId, AddressReadError> {
    Err(AddressReadError::NotServiceable(Capability::AddressHistory))
}
