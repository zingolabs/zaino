//! Daemon's validator set → [`zaino_traffic`] + [`zaino_chainview`]
//!
//! - Trust = `[[trusted_validators]]`, configured order (`chainview.md` §1: never discovered)
//! - One balancer: every request to them (polls, headers, blocks, lookups, submissions)
//! - Tip = their headers, trusted, in a header chain anchored at their tip − depth (in memory)

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::FutureExt as _;
use tracing::info;
use zaino_chainview::{ChainView, HeaderSync, ObservationFold, PeerWatch};
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::ReorgDepth;
use zaino_source::{IndexerWatch, ZebraRpcAdapter};
use zaino_traffic::{TrafficBalancer, TrafficDriver, Trusted, ValidatorId};

use crate::config::{DaemonConfig, TrustedValidatorConfig};
use crate::error::IndexerError;

/// View + the tasks driving it (spawn every one)
///
/// - `balancing` = the balancer's driver (every poll included); `fold` = polls into the view
/// - `watches`: push streams of each validator naming an `indexer_address`
/// - `peers`: `[p2p]` on = network start + the view's announcement fold
pub(crate) struct Wiring {
    pub(crate) view: Arc<ChainView<ZebraRpcAdapter>>,
    pub(crate) balancer: TrafficBalancer<ZebraRpcAdapter>,
    pub(crate) balancing: TrafficDriver<ZebraRpcAdapter>,
    pub(crate) fold: ObservationFold<ZebraRpcAdapter>,
    pub(crate) watches: Vec<(ValidatorId, IndexerWatch)>,
    pub(crate) header_sync: HeaderSync<ZebraRpcAdapter>,
    pub(crate) peers: Option<(BoxFuture<'static, ()>, PeerWatch)>,
}

/// One unprobed adapter per trusted validator, the balancer over them, the view over it (down at
/// boot = the balancer's retry, never a boot failure)
pub(crate) fn connect(config: &DaemonConfig) -> Result<Wiring, IndexerError> {
    let validators = &config.trusted_validators;
    let trusted = validators.iter().map(|validator| {
        let limits = validator.limits().ok_or_else(|| too_few_connections(validator))?;
        let source = Arc::new(adapter(validator)?);
        Ok(Trusted { source, priority: validator.priority, limits })
    });
    let trusted = trusted.collect::<Result<Vec<_>, IndexerError>>()?;
    let (balancer, balancing) = TrafficBalancer::new(trusted, None);
    let watches = validators.iter().enumerate().filter_map(|(index, validator)| {
        let address = validator.indexer_address.as_ref()?;
        let member = ValidatorId::new(index)?;
        Some(IndexerWatch::at(address, validator.into()).map(|watch| (member, watch)))
    });
    let watches = watches.collect::<Result<Vec<_>, _>>().map_err(IndexerError::from)?;
    let depth = ReorgDepth::new(config.sync.finalised_depth);
    let addresses = validators.iter().map(|validator| validator.jsonrpc_address.clone());
    let view = ChainView::new(addresses.collect(), balancer.clone(), depth)?;
    let mut view = view.with_submit_policy((&config.submission).into());
    let mut starting = None;
    if config.p2p.enabled {
        let (port, start) = crate::peers::start(config.p2p.peers(config.network));
        view = view.with_peers(port);
        starting = Some(start.boxed());
    }
    let peers = starting.zip(view.peer_watch());

    let header_sync = view.header_sync(HeaderChain::new(depth));
    let fold = view.observation_fold();
    let p2p = config.p2p.enabled;
    info!(validators = validators.len(), p2p, "Chain view configured");

    let view = Arc::new(view);
    Ok(Wiring { view, balancer, balancing, fold, watches, header_sync, peers })
}

/// [`DaemonConfig::validate`] refuses it first
fn too_few_connections(validator: &TrustedValidatorConfig) -> IndexerError {
    IndexerError::ConfigError(format!("{}: max_connections too low", validator.jsonrpc_address))
}

fn adapter(validator: &TrustedValidatorConfig) -> Result<ZebraRpcAdapter, IndexerError> {
    ZebraRpcAdapter::at(
        &validator.jsonrpc_address,
        validator.cookie_path.as_deref(),
        validator.user.clone(),
        validator.password.clone(),
        validator.into(),
        validator.link(),
    )
    .map_err(IndexerError::from)
}
