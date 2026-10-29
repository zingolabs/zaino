//! Daemon's validator set → [`zaino_traffic`] + [`zaino_chainview`]
//!
//! - Trust = `[[trusted_validators]]`, configured order, never discovered (architecture.md §3)
//! - One balancer: every request to them (polls, headers, blocks, lookups, submissions)
//! - Tip = their headers, trusted, in a header chain anchored at their tip − depth (in memory)

use std::sync::Arc;

use futures::future::BoxFuture;
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

use super::peers::{self, P2p};
use crate::config::{DaemonConfig, TrustedValidatorConfig};
use crate::error::IndexerError;

/// Balancer + the tasks driving it (spawn every one)
///
/// - View only once the upgrade schedule known ([`view`]: regtest's = a poll's)
/// - `balancing` = the balancer's driver (every poll included)
/// - `watches`: push streams of each validator naming an `indexer_address`
pub(crate) struct Wiring<S: ChainDataSource> {
    pub(crate) balancer: TrafficBalancer<S>,
    pub(crate) balancing: TrafficDriver<S>,
    pub(crate) watches: Vec<(TrustedValidatorId, ZebraIndexerWatch)>,
    pub(crate) p2p: Option<P2p>,
}

/// View + the tasks feeding it (spawn every one)
///
/// - `merge` = polls into the view
pub(crate) struct Viewed<S: ChainDataSource> {
    pub(crate) view: Arc<ChainView<S>>,
    pub(crate) merge: ObservationMerge<S>,
    pub(crate) header_sync: HeaderSync<S>,
    pub(crate) peers: Option<PeerTasks>,
}

/// `[p2p]` on, once viewed: the network's start + its block `inv` merge (one task)
pub(crate) struct PeerTasks {
    pub(crate) start: BoxFuture<'static, ()>,
    pub(crate) watch: PeerWatch,
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
            address: validator.endpoint.address.clone(),
            source,
            priority: validator.priority,
            limits: validator.max_connections,
        })
    });
    let trusted = trusted.collect::<Result<Vec<_>, IndexerError>>()?;
    let p2p = config.p2p.clone().map(peers::start);
    let peers = config.p2p.as_ref().zip(p2p.as_ref()).map(|(peer_config, p2p)| Peers {
        transport: Arc::clone(&p2p.port) as Arc<dyn PeerTransport>,
        in_flight: u32::try_from(peer_config.peer_target.get()).unwrap_or(u32::MAX),
    });
    let (balancer, balancing) = TrafficBalancer::new(trusted, peers);
    let watches = validators.iter().enumerate().filter_map(|(index, validator)| {
        let indexer = validator.indexer.as_ref()?;
        let id = TrustedValidatorId::new(index)?;
        let timeouts = validator.endpoint.timeouts;
        let watch = ZebraIndexerWatch::new(&indexer.address, indexer.tls.as_ref(), timeouts);
        Some(watch.map(|watch| (id, watch)))
    });
    let watches = watches.collect::<Result<Vec<_>, _>>().map_err(IndexerError::from)?;
    Ok(Wiring { balancer, balancing, watches, p2p })
}

/// The view over `balancer` for `schedule`, and the tasks feeding it
pub(crate) fn view<S: ChainDataSource>(
    config: &DaemonConfig,
    balancer: &TrafficBalancer<S>,
    p2p: Option<P2p>,
    schedule: LocalNetwork,
    shutdown: CancellationToken,
) -> Viewed<S> {
    let depth = FinalityDepth::new(config.sync.finalised_depth);
    let view = ChainView::new(balancer.clone(), depth, schedule, shutdown);
    let mut view = view.with_submit_policy(config.submission);
    let mut peers = None;
    if let Some(P2p { port, start }) = p2p {
        view = view.with_peers(port as Arc<dyn PeerSource>);
        peers = view.peer_watch().map(|watch| PeerTasks { start, watch });
    }

    let header_sync = view.header_sync(HeaderChain::new(depth));
    let merge = view.observation_merge();
    let p2p = config.p2p.is_some();
    info!(validators = config.trusted_validators.len(), p2p, "Chain view configured");

    Viewed { view: Arc::new(view), merge, header_sync, peers }
}

/// `validator_client = "legacy_rpc"`: every question over its JSON-RPC
pub(crate) fn legacy_client(
    validator: &TrustedValidatorConfig,
) -> Result<ZcashLegacyRpcClient, IndexerError> {
    ZcashLegacyRpcClient::new(&validator.endpoint).map_err(IndexerError::from)
}

/// `validator_client = "grpc"`: blocks over its indexer gRPC, the rest over its JSON-RPC
///
/// - the one check that every validator names an `indexer_address` ([`connect`] = boot, before
///   any download)
pub(crate) fn grpc_client(
    validator: &TrustedValidatorConfig,
) -> Result<ZebraIndexerGrpcClient, IndexerError> {
    let endpoint = &validator.endpoint;
    let indexer = validator.indexer.as_ref().ok_or_else(|| {
        IndexerError::Config(format!(
            "validator_client = \"grpc\" but [[trusted_validators]] {} has no \
             indexer_address (zebrad's indexer_listen_addr)",
            endpoint.address
        ))
    })?;
    let legacy = legacy_client(validator)?;
    let (address, tls) = (&indexer.address, indexer.tls.as_ref());
    ZebraIndexerGrpcClient::new(legacy, address, tls, endpoint.timeouts, endpoint.limits)
        .map_err(IndexerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DaemonConfigFile, TrustedValidatorTable};

    /// - No `indexer_address` = refused, naming the validator
    /// - `indexer_address` set = client (unprobed)
    // tokio::test: the gRPC channel's connect_lazy needs a runtime, no .await
    #[tokio::test]
    async fn the_grpc_client_needs_each_validators_indexer_address() {
        let mut indexed = TrustedValidatorTable::default();
        indexed.jsonrpc_address = "127.0.0.1:8231".to_owned();
        indexed.indexer_address = Some("127.0.0.1:8230".to_owned());
        let mut file = DaemonConfigFile::default();
        file.trusted_validators = vec![TrustedValidatorTable::default(), indexed];
        let resolved = file.resolve().expect("resolves").trusted_validators;
        let [unindexed, indexed] = &resolved[..] else { panic!("two validators: {resolved:?}") };

        let refused = grpc_client(unindexed).err().map(|e| e.to_string());
        let named = "127.0.0.1:8232 has no indexer_address";
        assert!(refused.as_deref().is_some_and(|refused| refused.contains(named)), "{refused:?}");

        let built = grpc_client(indexed).err().map(|e| e.to_string());
        assert_eq!(built, None, "indexer_address set: built unprobed (no validator needed)");
    }
}
