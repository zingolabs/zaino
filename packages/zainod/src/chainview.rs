//! Binds the daemon's validator set to [`zaino_chainview`], and supplies the serving layer's
//! compact projection.
//!
//! Membership = `[[trusted_validators]]`, in configured order (`docs/design/chainview.md` §1:
//! voters configured, never discovered). One validator is a threshold-1 quorum, so the wiring is
//! unconditional.

use std::sync::Arc;

use tonic::Status;
use tracing::info;
use zaino_chainview::{ChainView, EndpointPoller};
use zaino_grpc::{ChainViewHandles, ProjectCompact};
use zaino_index_compact_block::compact_tx;
use zaino_primitives::types::{ReorgDepth, Zatoshis};
use zaino_proto::proto::compact_formats::CompactTx;
use zaino_source::{decode_transaction, ZebraRpcAdapter};

use crate::config::{DaemonConfig, TrustedValidatorConfig};
use crate::error::IndexerError;

/// The view's endpoints, paired with the pollers that drive them.
pub(crate) struct Wiring {
    pub handles: ChainViewHandles,
    pub pollers: Vec<EndpointPoller<ZebraRpcAdapter>>,
    /// Configured order (one connection pool per validator, shared with fetch and serving)
    pub sources: Vec<Arc<ZebraRpcAdapter>>,
}

/// One unprobed adapter per trusted validator, and the view over them (a validator down at boot
/// is its poller's retry, never a boot failure)
pub(crate) fn connect(config: &DaemonConfig) -> Result<Wiring, IndexerError> {
    let endpoints = config
        .trusted_validators
        .iter()
        .map(|validator| Ok(endpoint(validator, Arc::new(adapter(validator)?))))
        .collect::<Result<Vec<_>, IndexerError>>()?;
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
    config: &TrustedValidatorConfig,
    source: Arc<ZebraRpcAdapter>,
) -> zaino_chainview::Endpoint<ZebraRpcAdapter> {
    zaino_chainview::Endpoint { address: config.jsonrpc_address.clone(), source }
}

fn adapter(validator: &TrustedValidatorConfig) -> Result<ZebraRpcAdapter, IndexerError> {
    ZebraRpcAdapter::at(
        &validator.jsonrpc_address,
        validator.cookie_path.as_deref(),
        validator.user.clone(),
        validator.password.clone(),
        validator.into(),
    )
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
