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

mod component;

pub use component::StoreComponent;

use std::future::Future;
use std::sync::Arc;

use zaino_core::{
    AddressBalance, AddressDelta, BlockId, Capability, Height, HeightRange, ServiceableRange,
    TransactionId, TransparentAddress, Utxo,
};
use zaino_indexes::indexes::address_history::{self, AddrId};
use zaino_indexes::indexes::headers::{HeaderValue, HeadersIndex, ID as HEADERS_ID};
use zaino_persistence::{Backend, BackendReader, Namespace};
use zaino_persistence_codec::{freshness, watermark, EntryCodec, Freshness};
use zaino_service::error::{AddressReadError, Transient};
use zaino_service::{AddressRead, Snapshot, TakeSnapshot};
use zaino_sync::primitives::BlockHeight;

/// EXPLORATORY: a read handle over the KV backend. It consumes the writer's
/// committed watermark on each snapshot — it holds no stubbed coordinates.
pub struct StoreReader<B> {
    backend: Arc<B>,
}

impl<B> StoreReader<B> {
    /// A reader over `backend`. The finalised tip is read live from the
    /// backend's watermark at snapshot time, not passed in.
    pub fn new(backend: Arc<B>) -> Self {
        Self { backend }
    }
}

impl<B: Backend + 'static> TakeSnapshot for StoreReader<B> {
    type Snapshot = StoreSnapshot<B>;

    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send {
        let backend = self.backend.clone();
        async move {
            // Consume the writer's watermark — the finalised tip this view can
            // answer up to — and pin it. (Reads through the snapshot still hit
            // live backend state; true read-coherence needs a backend read
            // transaction, which the in-memory stub lacks. See the crate banner.)
            let reader = backend
                .reader()
                .map_err(|e| Transient(format!("open reader: {e}")))?;
            let finalized_tip =
                watermark::read(&reader).map_err(|e| Transient(format!("read watermark: {e}")))?;
            // Compose the tip's BlockId on read from the headers index, so the
            // view reports a real (height, hash) rather than a bare height.
            let pinned_tip = match finalized_tip {
                Some(height) => read_header::<B>(&reader, height)?.map(|header| BlockId {
                    height,
                    hash: header.hash,
                }),
                None => None,
            };
            Ok(StoreSnapshot {
                backend: backend.clone(),
                finalized_tip,
                pinned_tip,
            })
        }
    }
}

/// EXPLORATORY: an immutable pinned view. Clones share the backend via `Arc`.
pub struct StoreSnapshot<B> {
    backend: Arc<B>,
    /// The finalised watermark this view was pinned to, read from the backend.
    finalized_tip: Option<Height>,
    /// The finalised tip's `BlockId`, composed from the headers index at pin time.
    pinned_tip: Option<BlockId>,
}

// Manual `Clone` so the bound is on `Arc<B>` (always cloneable), not `B`.
impl<B> Clone for StoreSnapshot<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            finalized_tip: self.finalized_tip,
            pinned_tip: self.pinned_tip,
        }
    }
}

impl<B: Backend + 'static> Snapshot for StoreSnapshot<B> {
    fn pinned_tip(&self) -> Option<BlockId> {
        self.pinned_tip
    }

    fn serviceable_range(&self) -> ServiceableRange {
        // No non-finalised window is wired, so the view answers up to the
        // finalised tip only: `tip == finalized_tip`.
        let tip = self.finalized_tip.unwrap_or(Height::GENESIS);
        ServiceableRange {
            finalized_tip: tip,
            tip,
        }
    }
}

/// Compose a block header on read from the headers index. EXPLORATORY: applies
/// the codec version guard — a format skew reads as absent rather than decoding
/// stale bytes.
fn read_header<B: Backend>(
    reader: &B::Reader,
    height: Height,
) -> Result<Option<HeaderValue>, Transient> {
    let namespace: Namespace = HEADERS_ID.into();
    if freshness::<HeadersIndex>(reader, namespace)
        .map_err(|e| Transient(format!("headers freshness: {e}")))?
        == Freshness::Stale
    {
        return Ok(None);
    }
    // The headers index is keyed by the engine's `BlockHeight`; convert from the
    // domain `Height` the watermark speaks.
    let key = HeadersIndex::encode_key(&BlockHeight::new(u64::from(height)));
    match reader
        .get(namespace, &key)
        .map_err(|e| Transient(format!("read header: {e}")))?
    {
        Some(bytes) => HeadersIndex::decode_value(&bytes)
            .map(Some)
            .map_err(|e| Transient(format!("decode header: {e}"))),
        None => Ok(None),
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
