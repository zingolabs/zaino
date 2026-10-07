//! Binds the daemon's validator set to [`zaino_chainview`].
//!
//! Trust = `[[trusted_validators]]`, in configured order (`docs/design/chainview.md` §1:
//! configured, never discovered); the tip = their headers, verified from genesis into the header
//! chain's store.

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::FutureExt as _;
use tracing::info;
use zaino_chainview::{ChainView, EndpointPoller, HeaderSync, PeerWatch};
use zaino_header_chain::{HeaderChain, HeaderStore, Params};
use zaino_persistence::fs::Fs;
use zaino_primitives::types::{Height, ReorgDepth};
use zaino_source::{IndexerWatch, Lane, ZebraRpcAdapter};
use zcash_protocol::consensus::NetworkType;

use crate::config::{DaemonConfig, TrustedValidatorConfig};
use crate::error::IndexerError;

/// The view's endpoints, paired with the pollers that drive them.
pub(crate) struct Wiring {
    pub(crate) view: Arc<ChainView<ZebraRpcAdapter>>,
    /// Each with its validator's push streams, if `indexer_address` names them
    pub(crate) pollers: Vec<(EndpointPoller<ZebraRpcAdapter>, Option<IndexerWatch>)>,
    /// Feeds the view its verified tip (run it beside the pollers)
    pub(crate) header_sync: HeaderSync<ZebraRpcAdapter>,
    /// Configured order (one connection pool per validator, shared with fetch and serving)
    pub(crate) sources: Vec<Arc<ZebraRpcAdapter>>,
    /// `[p2p]` on: the network's start + the view's announcement fold (spawn both)
    pub(crate) peers: Option<(BoxFuture<'static, ()>, PeerWatch)>,
}

/// One unprobed adapter per trusted validator, and the view over them (a validator down at boot
/// is its poller's retry, never a boot failure)
pub(crate) fn connect(config: &DaemonConfig, fs: Arc<dyn Fs>) -> Result<Wiring, IndexerError> {
    let endpoints = config
        .trusted_validators
        .iter()
        .map(|validator| Ok(endpoint(validator, Arc::new(adapter(validator)?))))
        .collect::<Result<Vec<_>, IndexerError>>()?;
    let sources: Vec<Arc<ZebraRpcAdapter>> =
        endpoints.iter().map(|e| Arc::clone(&e.source)).collect();
    let watches = config
        .trusted_validators
        .iter()
        .map(|validator| {
            let Some(address) = &validator.indexer_address else { return Ok(None) };
            Ok(Some(IndexerWatch::at(address, validator.into()).map_err(IndexerError::from)?))
        })
        .collect::<Result<Vec<_>, IndexerError>>()?;
    let depth = ReorgDepth::new(config.fetch.finalised_depth);
    let (view, pollers) = ChainView::new(endpoints, depth)?;
    let mut view = view.with_submit_policy((&config.submission).into());
    let mut starting = None;
    if config.p2p.enabled {
        let (port, start) = crate::peers::start(config.p2p.peers(config.network));
        view = view.with_peers(port);
        starting = Some(start.boxed());
    }
    let peers = starting.zip(view.peer_watch());
    let pollers = pollers.into_iter().zip(watches).collect();

    let headers = header_chain(config, fs, depth)?;
    let resumed = headers.final_tip().map_or(0, |tip| u32::from(tip.height));
    let bulk = sources.iter().map(|source| Arc::new(source.on(Lane::Sync))).collect();
    let header_sync = view.header_sync(headers, bulk);
    let validators = config.trusted_validators.len();
    let p2p = config.p2p.enabled;
    info!(validators, p2p, headers_final = resumed, "Chain view configured");

    Ok(Wiring { view: Arc::new(view), pollers, header_sync, sources, peers })
}

/// The verified header chain, resumed from its store (verified from genesis once, never a
/// checkpoint someone supplied)
///
/// - regtest: proof of work off (as zebrad's), Blossom spacing from height 1, NU7 unset (nBits =
///   the limit at every height, so spacing and window never decide anything there)
fn header_chain(
    config: &DaemonConfig,
    fs: Arc<dyn Fs>,
    depth: ReorgDepth,
) -> Result<HeaderChain, IndexerError> {
    let params = match config.network {
        NetworkType::Main => Params::mainnet(),
        NetworkType::Test => Params::testnet(),
        NetworkType::Regtest => Params::regtest(Height::GENESIS.next(), None),
    };
    let store = HeaderStore::open(fs, &config.index.header_chain.path, config.network)?;
    Ok(HeaderChain::open(params, depth, store))
}

fn endpoint(
    config: &TrustedValidatorConfig,
    source: Arc<ZebraRpcAdapter>,
) -> zaino_chainview::Endpoint<ZebraRpcAdapter> {
    zaino_chainview::Endpoint { address: config.jsonrpc_address.clone(), source }
}

/// On [`Lane::Control`](zaino_source::Lane): the poller's, and submission's
fn adapter(validator: &TrustedValidatorConfig) -> Result<ZebraRpcAdapter, IndexerError> {
    let limits = validator.limits().ok_or_else(|| {
        IndexerError::ConfigError(format!("{}: max_connections too low", validator.jsonrpc_address))
    })?;
    ZebraRpcAdapter::at(
        &validator.jsonrpc_address,
        validator.cookie_path.as_deref(),
        validator.user.clone(),
        validator.password.clone(),
        validator.into(),
        limits,
    )
    .map_err(IndexerError::from)
}
