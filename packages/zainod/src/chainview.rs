//! Daemon's validator set → [`zaino_traffic`] + [`zaino_chainview`]
//!
//! - Trust = `[[trusted_validators]]`, configured order, never discovered (architecture.md §3)
//! - One balancer: every request to them (polls, headers, blocks, lookups, submissions)
//! - Tip = their headers, trusted, in a header chain anchored at their tip − depth (in memory)

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::FutureExt as _;
use tracing::info;
use zaino_chainview::{ChainView, HeaderSync, ObservationFold, PeerWatch, ValidatorP2pSource};
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::{ReorgDepth, UpgradeSchedule};
use zaino_source::{ChainDataSource, IndexerWatch, ZcashGrpcClient, ZcashLegacyRpcClient};
use zaino_traffic::{PeerTransport, Peers, TrafficBalancer, TrafficDriver, Trusted, ValidatorId};

use crate::config::{DaemonConfig, TrustedValidatorConfig};
use crate::error::IndexerError;
use crate::peers::ZebraPeers;

/// Balancer + the tasks driving it (spawn every one); the view comes once the upgrade schedule
/// is known ([`view`]: regtest's is a poll's)
///
/// - `balancing` = the balancer's driver (every poll included)
/// - `watches`: push streams of each validator naming an `indexer_address`
/// - `p2p`: `[p2p]` on = the port + the future starting the network behind it
pub(crate) struct Wiring<S: ChainDataSource> {
    pub(crate) balancer: TrafficBalancer<S>,
    pub(crate) balancing: TrafficDriver<S>,
    pub(crate) watches: Vec<(ValidatorId, IndexerWatch)>,
    pub(crate) p2p: Option<(Arc<ZebraPeers>, BoxFuture<'static, ()>)>,
}

/// View + the tasks feeding it (spawn every one)
///
/// - `fold` = polls into the view; `peers`: `[p2p]` on = network start + block `inv` fold
pub(crate) struct Viewed<S: ChainDataSource> {
    pub(crate) view: Arc<ChainView<S>>,
    pub(crate) fold: ObservationFold<S>,
    pub(crate) header_sync: HeaderSync<S>,
    pub(crate) peers: Option<(BoxFuture<'static, ()>, PeerWatch)>,
}

/// One unprobed `client` per trusted validator ([`legacy_client`] / [`grpc_client`]) and the
/// balancer over them (down at boot = the balancer's retry, never a boot failure)
pub(crate) fn connect<S: ChainDataSource>(
    config: &DaemonConfig,
    client: fn(&TrustedValidatorConfig) -> Result<S, IndexerError>,
) -> Result<Wiring<S>, IndexerError> {
    let validators = &config.trusted_validators;
    let trusted = validators.iter().map(|validator| {
        let limits = validator.limits().ok_or_else(|| too_few_connections(validator))?;
        let source = Arc::new(client(validator)?);
        Ok(Trusted { source, priority: validator.priority, limits })
    });
    let trusted = trusted.collect::<Result<Vec<_>, IndexerError>>()?;
    let p2p = config.p2p.enabled.then(|| crate::peers::start(config.p2p.peers(config.network)));
    let peers = p2p.as_ref().map(|(port, _)| Peers {
        transport: Arc::clone(port) as Arc<dyn PeerTransport>,
        in_flight: u32::try_from(config.p2p.peer_target.get()).unwrap_or(u32::MAX),
    });
    let (balancer, balancing) = TrafficBalancer::new(trusted, peers);
    let watches = validators.iter().enumerate().filter_map(|(index, validator)| {
        let address = validator.indexer_address.as_ref()?;
        let member = ValidatorId::new(index)?;
        let tls = validator.indexer_tls();
        let watch = IndexerWatch::at(address, tls.as_ref(), validator.into());
        Some(watch.map(|watch| (member, watch)))
    });
    let watches = watches.collect::<Result<Vec<_>, _>>().map_err(IndexerError::from)?;
    let p2p = p2p.map(|(port, start)| (port, start.boxed()));
    Ok(Wiring { balancer, balancing, watches, p2p })
}

/// The view over `balancer` for `schedule`, and the tasks feeding it
pub(crate) fn view<S: ChainDataSource>(
    config: &DaemonConfig,
    balancer: &TrafficBalancer<S>,
    p2p: Option<(Arc<ZebraPeers>, BoxFuture<'static, ()>)>,
    schedule: UpgradeSchedule,
) -> Result<Viewed<S>, IndexerError> {
    let validators = &config.trusted_validators;
    let depth = ReorgDepth::new(config.sync.finalised_depth);
    let addresses = validators.iter().map(|validator| validator.jsonrpc_address.clone());
    let view = ChainView::new(addresses.collect(), balancer.clone(), depth, schedule)?;
    let mut view = view.with_submit_policy((&config.submission).into());
    let mut starting = None;
    if let Some((port, start)) = p2p {
        view = view.with_peers(port as Arc<dyn ValidatorP2pSource>);
        starting = Some(start);
    }
    let peers = starting.zip(view.peer_watch());

    let header_sync = view.header_sync(HeaderChain::new(depth));
    let fold = view.observation_fold();
    let p2p = config.p2p.enabled;
    info!(validators = validators.len(), p2p, "Chain view configured");

    Ok(Viewed { view: Arc::new(view), fold, header_sync, peers })
}

/// [`DaemonConfig::validate`] refuses it first
fn too_few_connections(validator: &TrustedValidatorConfig) -> IndexerError {
    IndexerError::ConfigError(format!("{}: max_connections too low", validator.jsonrpc_address))
}

/// `validator_client = "legacy_rpc"`: every question over its JSON-RPC
pub(crate) fn legacy_client(
    validator: &TrustedValidatorConfig,
) -> Result<ZcashLegacyRpcClient, IndexerError> {
    ZcashLegacyRpcClient::at(
        &validator.jsonrpc_address,
        validator.cookie_path.as_deref(),
        validator.user.clone(),
        validator.password.clone(),
        validator.into(),
        validator.link(),
    )
    .map_err(IndexerError::from)
}

/// `validator_client = "grpc"`: blocks over its indexer gRPC, the rest over its JSON-RPC
///
/// - [`DaemonConfig::validate`] refuses a validator without `indexer_address` first
pub(crate) fn grpc_client(
    validator: &TrustedValidatorConfig,
) -> Result<ZcashGrpcClient, IndexerError> {
    let indexer = validator.indexer_address.as_deref().ok_or_else(|| {
        IndexerError::ConfigError(format!("{}: no indexer_address", validator.jsonrpc_address))
    })?;
    let legacy = legacy_client(validator)?;
    let tls = validator.indexer_tls();
    ZcashGrpcClient::at(legacy, indexer, tls.as_ref(), validator.into(), validator.link())
        .map_err(IndexerError::from)
}
