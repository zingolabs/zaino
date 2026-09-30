//! Binds the daemon's validator set to [`zaino_chainview`], and supplies the serving layer's
//! compact projection.
//!
//! Membership = `source` + `chainview_peers`, in that order, so endpoint 0 is the validator the
//! indexes are built from (`docs/design/chainview.md` §1: operator-configured, never discovered).
//! A single-endpoint deployment is a threshold-1 quorum, so the wiring is unconditional.

use std::sync::Arc;

use tonic::Status;
use tracing::info;
use zaino_chainview::{ChainView, EndpointPoller};
use zaino_grpc::{ChainViewHandles, ProjectCompact};
use zaino_index_compact_block::compact_tx;
use zaino_primitives::types::{ReorgDepth, Zatoshis};
use zaino_proto::proto::compact_formats::CompactTx;
use zaino_source::{decode_transaction, ZebraRpcAdapter};

use crate::config::{DaemonConfig, SourceConfig};
use crate::error::IndexerError;

/// The view's endpoints, paired with the pollers that drive them.
pub(crate) struct Wiring {
    pub handles: ChainViewHandles,
    pub pollers: Vec<EndpointPoller<ZebraRpcAdapter>>,
    /// [`DaemonConfig::validators`] order (one connection per validator, shared with fetch)
    pub sources: Vec<Arc<ZebraRpcAdapter>>,
}

/// Dial every configured peer, then build the view over them and the primary.
///
/// `primary` is reused rather than re-dialled — it is the same validator, and a second
/// connection would double the poll load on the node the indexes already depend on.
pub(crate) async fn connect(
    primary: Arc<ZebraRpcAdapter>,
    config: &DaemonConfig,
) -> Result<Wiring, IndexerError> {
    let mut endpoints = vec![endpoint(&config.source, primary)];
    for peer in &config.chainview_peers {
        endpoints.push(endpoint(peer, Arc::new(dial(peer).await?)));
    }
    let sources = endpoints.iter().map(|e| Arc::clone(&e.source)).collect();
    let (view, pollers) = ChainView::new(endpoints, ReorgDepth::new(config.fetch.finalised_depth))?;

    let quorum = view.subscriber().quorum();
    info!(endpoints = quorum.configured(), threshold = quorum.threshold(), "Quorum configured");

    let view = Arc::new(view);
    Ok(Wiring {
        handles: ChainViewHandles {
            view: view.subscriber(),
            relay: view,
            compact: Arc::new(ZebraCompact),
        },
        pollers,
        sources,
    })
}

fn endpoint(
    config: &SourceConfig,
    source: Arc<ZebraRpcAdapter>,
) -> zaino_chainview::Endpoint<ZebraRpcAdapter> {
    zaino_chainview::Endpoint { address: config.jsonrpc_address.clone(), source }
}

async fn dial(peer: &SourceConfig) -> Result<ZebraRpcAdapter, IndexerError> {
    ZebraRpcAdapter::connect(
        &peer.jsonrpc_address,
        peer.cookie_path.as_deref(),
        peer.user.clone(),
        peer.password.clone(),
    )
    .await
    .map_err(IndexerError::from)
}

/// The consensus parse the serving layer cannot do for itself.
struct ZebraCompact;

impl ProjectCompact for ZebraCompact {
    fn project(&self, index: u64, raw: &[u8], fee: Option<Zatoshis>) -> Result<CompactTx, Status> {
        let parsed = decode_transaction(raw)
            .map_err(|failed| Status::internal(format!("mempool transaction: {failed}")))?;

        Ok(compact_tx(index, &parsed, fee))
    }
}
