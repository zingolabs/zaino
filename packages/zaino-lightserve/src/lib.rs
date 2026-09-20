//! `zaino-lightserve` — POC lightwalletd-compatible gRPC serve adapter.
//!
//! Proof that the new trait algebra rebinds the serving layer cleanly. A handler
//! bound to [`LightServeService`] alone (not the god-traits) pins a snapshot,
//! reads domain types, converts **domain -> wire in the adapter** (see
//! [`wire`]), and maps errors by kind — a transient snapshot failure, a
//! not-yet-serviceable chain, and a domain broadcast rejection are three
//! different wire outcomes, not one fused transport error.
//!
//! The handler covers the compact-block serving path (`GetLatestBlock`,
//! `GetBlock`, `GetBlockRange`, `GetLightdInfo`, `SendTransaction`);
//! [`GrpcServer`] stands up a real tonic `CompactTxStreamer` server over it
//! ([`RunLoop`](zaino_component::RunLoop)), serving those and returning
//! `Status::unimplemented` for the rest of the generated (fixed lightwalletd)
//! contract until their handler methods exist.
#![forbid(unsafe_code)]

mod error;
mod grpc;
mod transport;
mod wire;

pub use error::ServeError;
pub use grpc::GrpcService;
pub use transport::{GrpcServeError, GrpcServer};

use futures::stream::{BoxStream, StreamExt};
use zaino_core::{BlockRef, HeightRange};
use zaino_proto::proto::compact_formats as compact;
use zaino_proto::proto::service as proto;
use zaino_service::{CompactBlockRead, LightServeService, Snapshot};

use crate::wire::{to_hex, ToWire};

/// Lightwalletd-compatible handler over a [`LightServeService`] engine.
#[derive(Clone)]
pub struct LightServe<S: LightServeService> {
    engine: S,
}

impl<S: LightServeService> LightServe<S> {
    pub fn new(engine: S) -> Self {
        Self { engine }
    }

    /// `GetLatestBlock`: the tip of the pinned best chain, as a wire `BlockId`.
    pub async fn get_latest_block(&self) -> Result<proto::BlockId, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(ServeError::NoBlocks)?;
        Ok(tip.to_wire())
    }

    /// `GetLightdInfo`: serving metadata + the current tip height.
    ///
    /// Minimal but valid: `version`/`vendor`/`taddr_support` are static, and
    /// `block_height`/`estimated_height` are read from the pinned tip (0 before
    /// any block is served). The network-derived fields (`chain_name`,
    /// `sapling_activation_height`, `consensus_branch_id`) are left best-effort
    /// empty here — the handler is not parameterised by the network, and the
    /// clients that gate readiness on this call read only the height. Threading
    /// the network through to fill them is a later refinement.
    pub async fn get_lightd_info(&self) -> Result<proto::LightdInfo, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let block_height = snapshot
            .pinned_tip()
            .map(|tip| u64::from(tip.height))
            .unwrap_or(0);
        Ok(proto::LightdInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            vendor: "zaino".to_string(),
            taddr_support: true,
            block_height,
            estimated_height: block_height,
            ..Default::default()
        })
    }

    /// `GetBlock`: the composed compact block at `at`, or `None` when no block
    /// is indexed there (the caller maps that to a not-found status). The
    /// snapshot pins the view; the compact block is composed on read and
    /// converted domain -> wire in the adapter.
    pub async fn get_block(
        &self,
        at: BlockRef,
    ) -> Result<Option<compact::CompactBlock>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        Ok(snapshot.compact_block(at).await?.map(ToWire::to_wire))
    }

    /// `GetBlockRange`: the composed compact blocks over `range`, as a stream.
    ///
    /// The store composes the range eagerly, so the items are collected owned
    /// and returned as a `'static` stream; a per-block read failure rides the
    /// stream, classified by kind, rather than aborting the pin. Acquiring the
    /// snapshot itself can still fail up front (transient).
    pub async fn get_block_range(
        &self,
        range: HeightRange,
    ) -> Result<BoxStream<'static, Result<compact::CompactBlock, ServeError>>, ServeError> {
        let snapshot = self.engine.snapshot().await?;
        let blocks: Vec<Result<compact::CompactBlock, ServeError>> = snapshot
            .stream_compact(range)
            .map(|read| read.map(ToWire::to_wire).map_err(ServeError::from))
            .collect()
            .await;
        Ok(Box::pin(futures::stream::iter(blocks)))
    }

    /// `SendTransaction`: relay raw bytes. A rejection is a domain answer, so it
    /// rides out in the `SendResponse` (non-zero `error_code`), not as an error.
    pub async fn send_transaction(&self, raw: proto::RawTransaction) -> proto::SendResponse {
        match self.engine.broadcast(raw.data.to_vec()).await {
            Ok(txid) => proto::SendResponse {
                error_code: 0,
                error_message: to_hex(txid.into()),
            },
            Err(rejection) => proto::SendResponse {
                error_code: -1,
                error_message: format!("{rejection:?}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LightServe, ServeError};
    use zaino_core::{BlockHash, BlockId, Height};
    use zaino_proto::proto::service as proto;
    use zaino_service::testing::{MockChain, MockIndexerService};

    fn engine_with_tip(tip: Option<BlockId>) -> MockIndexerService {
        MockIndexerService::new(MockChain {
            tip,
            ..Default::default()
        })
    }

    /// The handler binds only `LightServeService`, pins a snapshot, and converts
    /// the domain tip to a wire `BlockId`.
    #[tokio::test]
    async fn latest_block_maps_domain_to_wire() {
        let tip = BlockId {
            height: Height::try_from(808).expect("valid height"),
            hash: BlockHash::from([0xABu8; 32]),
        };
        let serve = LightServe::new(engine_with_tip(Some(tip)));

        let wire = serve.get_latest_block().await.expect("latest block");
        assert_eq!(wire.height, 808u64);
        assert_eq!(wire.hash, vec![0xABu8; 32]);
    }

    /// An empty chain is `NoBlocks` (a serviceability fact), not a transport error.
    #[tokio::test]
    async fn latest_block_no_tip_is_no_blocks() {
        let serve = LightServe::new(engine_with_tip(None));
        assert!(matches!(
            serve.get_latest_block().await,
            Err(ServeError::NoBlocks)
        ));
    }

    /// `GetLightdInfo` reports the tip height and succeeds once the chain has a
    /// tip — the readiness signal clients gate on.
    #[tokio::test]
    async fn lightd_info_reports_the_tip_height() {
        let tip = BlockId {
            height: Height::try_from(42).expect("valid height"),
            hash: BlockHash::from([0x11u8; 32]),
        };
        let serve = LightServe::new(engine_with_tip(Some(tip)));
        let info = serve.get_lightd_info().await.expect("lightd info");
        assert_eq!(info.block_height, 42u64);
        assert_eq!(info.estimated_height, 42u64);
        assert!(info.taddr_support);
        assert!(!info.version.is_empty());
    }

    /// Before any block is served, `GetLightdInfo` still succeeds with height 0
    /// (a valid answer, not `NoBlocks`) — the call itself is the liveness gate.
    #[tokio::test]
    async fn lightd_info_before_any_block_is_height_zero() {
        let serve = LightServe::new(engine_with_tip(None));
        let info = serve.get_lightd_info().await.expect("lightd info");
        assert_eq!(info.block_height, 0u64);
    }

    /// A successful broadcast returns `error_code == 0` with the txid in hex.
    #[tokio::test]
    async fn send_transaction_success_is_a_response() {
        let serve = LightServe::new(engine_with_tip(None));
        let resp = serve
            .send_transaction(proto::RawTransaction {
                data: vec![1, 2, 3].into(),
                height: 0,
            })
            .await;
        assert_eq!(resp.error_code, 0);
        assert_eq!(resp.error_message, "0".repeat(64)); // mock returns the zero txid
    }
}
