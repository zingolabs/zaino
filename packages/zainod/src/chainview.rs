//! Daemon's validator set → [`zaino_traffic`] + [`zaino_chainview`]
//!
//! - Trust = `[[trusted_validators]]`, configured order, never discovered (architecture.md §3)
//! - One balancer: every request to them (polls, headers, blocks, lookups, submissions)
//! - Tip = their headers, trusted, in a header chain anchored at their tip − depth (in memory)

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::FutureExt as _;
use tokio_util::sync::CancellationToken;
use tracing::info;
use zaino_chainview::{ChainView, HeaderSync, ObservationMerge, PeerSource, PeerWatch};
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::FinalityDepth;
use zaino_source::{
    ChainDataSource, ZcashLegacyRpcClient, ZebraIndexerGrpcClient, ZebraIndexerWatch,
};
use zaino_traffic::{
    PeerTransport, Peers, TrafficBalancer, TrafficDriver, TrustedValidator, TrustedValidatorId,
};
use zcash_protocol::local_consensus::LocalNetwork;

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
    pub(crate) watches: Vec<(TrustedValidatorId, ZebraIndexerWatch)>,
    pub(crate) p2p: Option<(Arc<ZebraPeers>, BoxFuture<'static, ()>)>,
}

/// View + the tasks feeding it (spawn every one)
///
/// - `merge` = polls into the view; `peers`: `[p2p]` on = network start + block `inv` merge
pub(crate) struct Viewed<S: ChainDataSource> {
    pub(crate) view: Arc<ChainView<S>>,
    pub(crate) merge: ObservationMerge<S>,
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
        let source = Arc::new(client(validator)?);
        Ok(TrustedValidator {
            source,
            priority: validator.priority,
            limits: validator.max_connections,
        })
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
        let id = TrustedValidatorId::new(index)?;
        let watch =
            ZebraIndexerWatch::new(address, validator.indexer_tls.as_ref(), validator.timeouts());
        Some(watch.map(|watch| (id, watch)))
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
    schedule: LocalNetwork,
    shutdown: CancellationToken,
) -> Result<Viewed<S>, IndexerError> {
    let validators = &config.trusted_validators;
    let depth = FinalityDepth::new(config.sync.finalised_depth);
    let addresses = validators.iter().map(|validator| validator.jsonrpc_address.clone());
    let view = ChainView::new(addresses.collect(), balancer.clone(), depth, schedule, shutdown)?;
    let mut view = view.with_submit_policy(config.submission);
    let mut starting = None;
    if let Some((port, start)) = p2p {
        view = view.with_peers(port as Arc<dyn PeerSource>);
        starting = Some(start);
    }
    let peers = starting.zip(view.peer_watch());

    let header_sync = view.header_sync(HeaderChain::new(depth));
    let merge = view.observation_merge();
    let p2p = config.p2p.enabled;
    info!(validators = validators.len(), p2p, "Chain view configured");

    Ok(Viewed { view: Arc::new(view), merge, header_sync, peers })
}

/// `validator_client = "legacy_rpc"`: every question over its JSON-RPC
pub(crate) fn legacy_client(
    validator: &TrustedValidatorConfig,
) -> Result<ZcashLegacyRpcClient, IndexerError> {
    ZcashLegacyRpcClient::new(
        &validator.jsonrpc_address,
        validator.cookie_path.as_deref(),
        validator.user.clone(),
        validator.password.clone(),
        validator.timeouts(),
        validator.link(),
    )
    .map_err(IndexerError::from)
}

/// `validator_client = "grpc"`: blocks over its indexer gRPC, the rest over its JSON-RPC
///
/// - the one check that every validator names an `indexer_address` ([`connect`] = boot, before
///   any download)
pub(crate) fn grpc_client(
    validator: &TrustedValidatorConfig,
) -> Result<ZebraIndexerGrpcClient, IndexerError> {
    let address = &validator.jsonrpc_address;
    let indexer = validator.indexer_address.as_deref().ok_or_else(|| {
        IndexerError::Config(format!(
            "validator_client = \"grpc\" but [[trusted_validators]] {address} has no \
             indexer_address (zebrad's indexer_listen_addr)"
        ))
    })?;
    let legacy = legacy_client(validator)?;
    ZebraIndexerGrpcClient::new(
        legacy,
        indexer,
        validator.indexer_tls.as_ref(),
        validator.timeouts(),
        validator.link(),
    )
    .map_err(IndexerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No `indexer_address` = refused, naming the validator; one set = a client (unprobed)
    #[tokio::test]
    async fn the_grpc_client_needs_each_validators_indexer_address() {
        let refused = grpc_client(&TrustedValidatorConfig::default()).err().map(|e| e.to_string());
        let named = "127.0.0.1:8232 has no indexer_address";
        assert!(refused.as_deref().is_some_and(|refused| refused.contains(named)), "{refused:?}");

        let indexed = TrustedValidatorConfig {
            indexer_address: Some("127.0.0.1:8230".to_owned()),
            ..TrustedValidatorConfig::default()
        };
        assert!(grpc_client(&indexed).is_ok(), "no validator needed to build one");
    }
}
