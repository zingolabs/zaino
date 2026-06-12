//! Zaino Testing Utilities.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use futures::StreamExt as _;
use once_cell::sync::Lazy;
use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};
#[cfg(test)]
use tonic::transport::Channel;
use tracing::{debug, info, instrument};
use zaino_common::{
    network::{ActivationHeights, ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS},
    probing::{Liveness, Readiness},
    status::Status,
    validator::ValidatorConfig,
    CacheConfig, DatabaseConfig, Network, ServiceConfig, StorageConfig,
};
use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::service::{BlockId, BlockRange};
use zaino_serve::server::config::{GrpcServerConfig, JsonRpcServerConfig};
use zaino_state::{
    BackendType, BlockchainSource, ChainIndex, FetchServiceSubscriber, LightWalletIndexer,
    LightWalletService, NodeBackedChainIndexSubscriber, StateServiceSubscriber, ZcashIndexer,
    ZcashService,
};
#[allow(deprecated)]
use zaino_state::{FetchService, FetchServiceConfig, StateService, StateServiceConfig};
use zainodlib::{config::ZainodConfig, error::IndexerError, indexer::Indexer};
pub use zcash_local_net as services;
use zcash_local_net::validator::zebrad::{Zebrad, ZebradConfig};
pub use zcash_local_net::validator::Validator;
use zcash_local_net::validator::ValidatorConfig as _;
use zcash_local_net::PoolType;
use zcash_local_net::{
    error::LaunchError,
    validator::zcashd::{Zcashd, ZcashdConfig},
};
use zcash_local_net::{logs::LogsToStdoutAndStderr, process::Process};
use zebra_chain::parameters::NetworkKind;
use zebra_rpc::methods::GetInfo;

#[cfg(test)]
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;

/// Generates one `#[tokio::test(flavor = "multi_thread")]` wrapper per
/// `name => helper` pair, each calling `helper::<$validator>(&$kind).await`.
///
/// Collapses the per-validator boilerplate in the `fetch_service` /
/// `state_service` test modules: invoke once per validator inside the relevant
/// `mod`, supplying that validator's test list. A macro (not a fn) because each
/// wrapper must be a discoverable `#[tokio::test]` item.
#[macro_export]
macro_rules! validator_tests {
    ($validator:ty, $kind:expr, $( $name:ident => $helper:ident ),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn $name() {
                $helper::<$validator>(&$kind).await;
            }
        )*
    };
}

/// Collect a `get_block_range` query over heights `[start, end]` for the given
/// proto `pool_types` into a vector of compact blocks. Generic over the
/// lightwallet subscriber, so it serves both `FetchServiceSubscriber` and
/// `StateServiceSubscriber`. Shared by the fetch_service and state_service
/// tests in both workspaces.
#[allow(deprecated)]
pub async fn collect_block_range<S: LightWalletIndexer>(
    subscriber: &S,
    start: u64,
    end: u64,
    pool_types: Vec<i32>,
) -> Vec<CompactBlock> {
    subscriber
        .get_block_range(BlockRange {
            start: Some(BlockId {
                height: start,
                hash: vec![],
            }),
            end: Some(BlockId {
                height: end,
                hash: vec![],
            }),
            pool_types,
        })
        .await
        .expect("get_block_range")
        .map(|block| block.expect("compact block in range"))
        .collect()
        .await
}

/// Drain a `get_block_range` query over heights `[start, end]` for `pool_types`,
/// collecting the compact blocks and reporting whether the stream terminated
/// with an error (`true`) rather than a clean end-of-stream (`false`). The
/// error-tolerant counterpart to [`collect_block_range`], for out-of-range
/// tests that expect the stream to error partway through.
#[allow(deprecated)]
pub async fn drain_block_range<S: LightWalletIndexer>(
    subscriber: &S,
    start: u64,
    end: u64,
    pool_types: Vec<i32>,
) -> (Vec<CompactBlock>, bool) {
    let mut stream = subscriber
        .get_block_range(BlockRange {
            start: Some(BlockId {
                height: start,
                hash: vec![],
            }),
            end: Some(BlockId {
                height: end,
                hash: vec![],
            }),
            pool_types,
        })
        .await
        .expect("get_block_range");
    let mut blocks = Vec::new();
    let mut errored = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(block) => blocks.push(block),
            Err(_) => {
                errored = true;
                break;
            }
        }
    }
    (blocks, errored)
}

/// Helper to get the test binary path from the TEST_BINARIES_DIR env var.
fn binary_path(binary_name: &str) -> Option<PathBuf> {
    std::env::var("TEST_BINARIES_DIR")
        .ok()
        .map(|dir| PathBuf::from(dir).join(binary_name))
}

/// Create local URI from port.
pub fn make_uri(indexer_port: portpicker::Port) -> http::Uri {
    format!("http://127.0.0.1:{indexer_port}")
        .try_into()
        .unwrap()
}

/// Polls until the given component reports ready.
///
/// Returns `true` if the component became ready within the timeout,
/// `false` if the timeout was reached.
#[instrument(name = "poll_until_ready", skip(component), fields(timeout_ms = timeout.as_millis() as u64))]
pub async fn poll_until_ready(
    component: &impl Readiness,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> bool {
    debug!("[POLL] Waiting for component to be ready");
    let result = tokio::time::timeout(timeout, async {
        let mut interval = tokio::time::interval(poll_interval);
        loop {
            interval.tick().await;
            if component.is_ready() {
                return;
            }
        }
    })
    .await
    .is_ok();
    if result {
        debug!("[POLL] Component is ready");
    } else {
        debug!("[POLL] Timeout waiting for component");
    }
    result
}

/// Source of "current tip height" for the unified block-and-wait helper.
///
/// Implementors are anything a test wants to wait on: a Zaino service
/// subscriber or a `NodeBackedChainIndexSubscriber`.
///
/// [`Status`] (and through it [`Liveness`] / [`Readiness`] via the blanket
/// impls in `zaino_common::status`) is a supertrait so a single
/// `T: PollableTip` bound is everything the unified helper needs — it can
/// poll for height, fail fast on a dead backend, and wait for readiness
/// once the target height is reached.
///
/// Impls are listed explicitly rather than blanket-impl'd over
/// `LightWalletIndexer` so the compiler can rule out coherence conflicts
/// with the `NodeBackedChainIndexSubscriber` impl (Rust's orphan rules
/// can't otherwise prove a foreign type won't grow an upstream
/// `LightWalletIndexer` impl). Add a new impl per pollable type.
pub trait PollableTip: Status + Sync {
    /// Current observable tip height, in absolute block-height units.
    fn tip_height(&self) -> impl std::future::Future<Output = u64>;
}

impl PollableTip for FetchServiceSubscriber {
    async fn tip_height(&self) -> u64 {
        self.get_latest_block()
            .await
            .expect("PollableTip: FetchServiceSubscriber::get_latest_block failed")
            .height
    }
}

impl PollableTip for StateServiceSubscriber {
    async fn tip_height(&self) -> u64 {
        self.get_latest_block()
            .await
            .expect("PollableTip: StateServiceSubscriber::get_latest_block failed")
            .height
    }
}

impl<Source: BlockchainSource> PollableTip for NodeBackedChainIndexSubscriber<Source> {
    async fn tip_height(&self) -> u64 {
        let snapshot = self
            .snapshot_nonfinalized_state()
            .await
            .expect("PollableTip: chain-index snapshot_nonfinalized_state failed");
        u64::from(u32::from(
            self.best_chaintip(&snapshot)
                .await
                .expect("PollableTip: chain-index best_chaintip failed")
                .height,
        ))
    }
}

/// Marker trait bundling the `Self`-relative constraints every
/// integration test needs on the `Service` type parameter of
/// [`TestManager`].
///
/// Lets a generic test function write `Service: TestService` instead
/// of restating these bounds in every `where`-clause:
/// - [`LightWalletService`] + `Send + Sync + 'static`
/// - `Service::Config: TryFrom<ZainodConfig, Error = IndexerError>`
/// - `Service::Subscriber: PollableTip`
///
/// **Not** bundled: the reverse bound
/// `IndexerError: From<Service::Subscriber::Error>`. Rust does not
/// propagate non-`Self` bounds declared in a trait's `where`-clause
/// through a `T: TestService` constraint, so call sites that touch
/// `TestManager::launch` (or anything else exercising
/// `Indexer::launch_inner`'s `?` propagation) must still restate that
/// one bound explicitly. Everything else collapses to `TestService`.
pub trait TestService:
    LightWalletService<Config: TryFrom<ZainodConfig, Error = IndexerError>, Subscriber: PollableTip>
    + Send
    + Sync
    + 'static
{
}

impl<T> TestService for T where
    T: LightWalletService<
            Config: TryFrom<ZainodConfig, Error = IndexerError>,
            Subscriber: PollableTip,
        > + Send
        + Sync
        + 'static
{
}

// temporary until activation heights are unified to zebra-chain type.
// from/into impls not added in zaino-common to avoid unecessary addition of zcash-protocol dep to non-test code
/// Convert zaino activation heights into zcash protocol type.
pub fn local_network_from_activation_heights(
    activation_heights: ActivationHeights,
) -> zcash_protocol::local_consensus::LocalNetwork {
    zcash_protocol::local_consensus::LocalNetwork {
        overwinter: activation_heights
            .overwinter
            .map(zcash_protocol::consensus::BlockHeight::from),
        sapling: activation_heights
            .sapling
            .map(zcash_protocol::consensus::BlockHeight::from),
        blossom: activation_heights
            .blossom
            .map(zcash_protocol::consensus::BlockHeight::from),
        heartwood: activation_heights
            .heartwood
            .map(zcash_protocol::consensus::BlockHeight::from),
        canopy: activation_heights
            .canopy
            .map(zcash_protocol::consensus::BlockHeight::from),
        nu5: activation_heights
            .nu5
            .map(zcash_protocol::consensus::BlockHeight::from),
        nu6: activation_heights
            .nu6
            .map(zcash_protocol::consensus::BlockHeight::from),
        nu6_1: activation_heights
            .nu6_1
            .map(zcash_protocol::consensus::BlockHeight::from),
        nu6_2: activation_heights
            .nu6_2
            .map(zcash_protocol::consensus::BlockHeight::from),
    }
}

/// Path for zcashd binary.
pub static ZCASHD_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| binary_path("zcashd"));

/// Path for zcash-cli binary.
pub static ZCASH_CLI_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| binary_path("zcash-cli"));

/// Path for zebrad binary.
pub static ZEBRAD_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| binary_path("zebrad"));

/// Path for lightwalletd binary.
pub static LIGHTWALLETD_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| binary_path("lightwalletd"));

/// Path for zainod binary.
pub static ZAINOD_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| binary_path("zainod"));

/// Path for zcashd chain cache.
pub static ZCASHD_CHAIN_CACHE_DIR: Lazy<Option<PathBuf>> = Lazy::new(|| {
    let mut workspace_root_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    workspace_root_path.pop();
    Some(workspace_root_path.join("integration-tests/chain_cache/client_rpc_tests"))
});

/// Path for zebrad chain cache.
pub static ZEBRAD_CHAIN_CACHE_DIR: Lazy<Option<PathBuf>> = Lazy::new(|| {
    let mut workspace_root_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    workspace_root_path.pop();
    Some(workspace_root_path.join("integration-tests/chain_cache/client_rpc_tests_large"))
});

/// Path for the Zebra chain cache in the user's home directory.
pub static ZEBRAD_TESTNET_CACHE_DIR: Lazy<Option<PathBuf>> = Lazy::new(|| {
    let home_path = PathBuf::from(std::env::var("HOME").unwrap());
    Some(home_path.join(".cache/zebra"))
});

#[derive(Debug, PartialEq, Clone, Copy)]
/// Represents the type of validator to launch.
pub enum ValidatorKind {
    /// Zcashd.
    Zcashd,
    /// Zebrad.
    Zebrad,
}

/// Config for validators.
pub enum ValidatorTestConfig {
    /// Zcashd Config.
    ZcashdConfig(ZcashdConfig),
    /// Zebrad Config.
    ZebradConfig(zcash_local_net::validator::zebrad::ZebradConfig),
}

/// Configuration data for Zaino Tests.
pub struct TestManager<C: Validator, Service: LightWalletService + Send + Sync + 'static> {
    /// Control plane for a validator
    pub local_net: C,
    /// Data directory for the validator.
    pub data_dir: PathBuf,
    /// Network (chain) type:
    pub network: NetworkKind,
    /// Zebrad/Zcashd JsonRpc listen address.
    pub full_node_rpc_listen_address: SocketAddr,
    /// Zebrad/Zcashd gRpc listen address.
    pub full_node_grpc_listen_address: SocketAddr,
    /// Zaino Indexer JoinHandle.
    pub zaino_handle: Option<tokio::task::JoinHandle<Result<(), IndexerError>>>,
    /// Zaino JsonRPC listen address.
    pub zaino_json_rpc_listen_address: Option<SocketAddr>,
    /// Zaino gRPC listen address.
    pub zaino_grpc_listen_address: Option<SocketAddr>,
    /// Service subscriber.
    pub service_subscriber: Option<Service::Subscriber>,
    /// JsonRPC server cookie dir.
    pub json_server_cookie_dir: Option<PathBuf>,
}

/// Needed validator functionality that is not implemented in infrastructure
///
/// TODO: Either move to Validator zcash_client_backend trait or document
/// why it should not be moved.
pub trait ValidatorExt: Validator + LogsToStdoutAndStderr {
    /// Launch the validator, and return a validator config containing the
    /// ports used by the validator, etc
    fn launch_validator_and_return_config(
        config: Self::Config,
    ) -> impl Future<Output = Result<(Self, ValidatorConfig), LaunchError>> + Send + Sync;
}

impl ValidatorExt for Zebrad {
    async fn launch_validator_and_return_config(
        config: ZebradConfig,
    ) -> Result<(Self, ValidatorConfig), LaunchError> {
        let zebrad = Zebrad::launch(config).await?;
        let validator_config = ValidatorConfig {
            validator_jsonrpc_listen_address: format!(
                "{}:{}",
                Ipv4Addr::LOCALHOST,
                zebrad.rpc_listen_port()
            ),
            validator_grpc_listen_address: Some(format!(
                "{}:{}",
                Ipv4Addr::LOCALHOST,
                zebrad.indexer_listen_port()
            )),
            validator_cookie_path: None,
            validator_user: Some("xxxxxx".to_string()),
            validator_password: Some("xxxxxx".to_string()),
        };
        Ok((zebrad, validator_config))
    }
}

impl ValidatorExt for Zcashd {
    async fn launch_validator_and_return_config(
        config: Self::Config,
    ) -> Result<(Self, ValidatorConfig), LaunchError> {
        let zcashd = Zcashd::launch(config).await?;
        let validator_config = ValidatorConfig {
            validator_jsonrpc_listen_address: format!("{}:{}", Ipv4Addr::LOCALHOST, zcashd.port()),
            validator_grpc_listen_address: None,
            validator_cookie_path: None,
            validator_user: Some("xxxxxx".to_string()),
            validator_password: Some("xxxxxx".to_string()),
        };
        Ok((zcashd, validator_config))
    }
}

impl<C, Service> TestManager<C, Service>
where
    C: ValidatorExt,
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    /// Returns the service subscriber, panicking if zaino wasn't enabled at launch.
    ///
    /// Convenience for tests that always pass `enable_zaino: true` and want to
    /// hand the subscriber to [`Self::generate_blocks_and_wait_for_tip`] without
    /// repeating `.service_subscriber.as_ref().unwrap()` at every call site.
    pub fn subscriber(&self) -> &Service::Subscriber {
        self.service_subscriber
            .as_ref()
            .expect("TestManager::subscriber called but service_subscriber is None (zaino disabled at launch?)")
    }

    #[cfg(test)]
    pub(crate) fn grpc_socket_to_uri(&self) -> http::Uri {
        http::Uri::builder()
            .scheme("http")
            .authority(
                self.zaino_grpc_listen_address
                    .expect("grpc_listen_address should be set")
                    .to_string(),
            )
            .path_and_query("/")
            .build()
            .unwrap()
    }

    /// Launches zcash-local-net<Empty, Validator>.
    ///
    /// Possible validators: Zcashd, Zebrad.
    ///
    /// If chain_cache is given a path the chain will be loaded.
    ///
    /// `enable_clients` must be `false`: zaino-testutils carries no zingolib, so
    /// wallet lightclients are built by the separate wallet-tests workspace from
    /// the returned `TestManager`'s gRPC address. Passing `true` returns an error.
    ///
    /// TODO: Add TestManagerConfig struct and constructor methods of common test setups.
    ///
    /// TODO: Remove validator argument in favour of adding C::VALIDATOR associated const
    #[instrument(
        name = "TestManager::launch",
        skip(activation_heights, chain_cache),
        fields(validator = ?validator, network = ?network, enable_zaino, enable_clients)
    )]
    pub async fn launch(
        validator: &ValidatorKind,
        network: Option<NetworkKind>,
        activation_heights: Option<ActivationHeights>,
        chain_cache: Option<PathBuf>,
        enable_zaino: bool,
        enable_zaino_jsonrpc_server: bool,
        enable_clients: bool,
    ) -> Result<Self, std::io::Error> {
        if (validator == &ValidatorKind::Zcashd) && (Service::BACKEND_TYPE == BackendType::State) {
            return Err(std::io::Error::other(
                "Cannot use state backend with zcashd.",
            ));
        }
        zaino_common::logging::try_init();

        let activation_heights = activation_heights.unwrap_or_else(|| match validator {
            ValidatorKind::Zcashd => ActivationHeights::default(),
            ValidatorKind::Zebrad => ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS,
        });
        let network_kind = network.unwrap_or(NetworkKind::Regtest);
        let zaino_network_kind =
            Network::from_network_kind_and_activation_heights(&network_kind, &activation_heights);

        if enable_clients && !enable_zaino {
            return Err(std::io::Error::other(
                "Cannot enable clients when zaino is not enabled.",
            ));
        }

        // Launch LocalNet:

        let mut config = C::Config::default();
        config.set_test_parameters(
            if validator == &ValidatorKind::Zebrad {
                PoolType::Transparent
            } else {
                PoolType::ORCHARD
            },
            activation_heights.into(),
            chain_cache.clone(),
        );

        debug!("[TEST] Launching validator");
        let (local_net, validator_settings) = C::launch_validator_and_return_config(config)
            .await
            .expect("to launch a default validator");
        let rpc_listen_port = local_net.get_port();
        debug!(rpc_port = rpc_listen_port, "[TEST] Validator launched");
        let full_node_rpc_listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_listen_port);

        let data_dir = local_net.data_dir().path().to_path_buf();
        let zaino_db_path = data_dir.join("zaino");

        let zebra_db_path = match chain_cache {
            Some(cache) => cache,
            None => data_dir.clone(),
        };

        // Launch Zaino:
        let (
            zaino_handle,
            zaino_service_subscriber,
            zaino_grpc_listen_address,
            zaino_json_listen_address,
            zaino_json_server_cookie_dir,
        ) = if enable_zaino {
            let zaino_grpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
            let zaino_grpc_listen_address =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zaino_grpc_listen_port);
            let zaino_json_listen_port = portpicker::pick_unused_port().expect("No ports free");
            let zaino_json_listen_address =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zaino_json_listen_port);
            debug!(
                grpc_address = %zaino_grpc_listen_address,
                json_address = %zaino_json_listen_address,
                "[TEST] Launching Zaino indexer"
            );
            let indexer_config = zainodlib::config::ZainodConfig {
                // TODO: Make configurable.
                backend: Service::BACKEND_TYPE,
                json_server_settings: if enable_zaino_jsonrpc_server {
                    Some(JsonRpcServerConfig {
                        json_rpc_listen_address: zaino_json_listen_address,
                        cookie_dir: None,
                    })
                } else {
                    None
                },
                grpc_settings: GrpcServerConfig {
                    listen_address: zaino_grpc_listen_address,
                    tls: None,
                },
                validator_settings: validator_settings.clone(),
                service: ServiceConfig::default(),
                storage: StorageConfig {
                    cache: CacheConfig::default(),
                    database: DatabaseConfig {
                        path: zaino_db_path,
                        ..Default::default()
                    },
                },
                zebra_db_path,
                network: zaino_network_kind,
                donation_address: None,
            };

            let (handle, service_subscriber) = Indexer::<Service>::launch_inner(
                Service::Config::try_from(indexer_config.clone())
                    .expect("Failed to convert ZainodConfig to service config"),
                indexer_config,
            )
            .await
            .unwrap();

            (
                Some(handle),
                Some(service_subscriber),
                Some(zaino_grpc_listen_address),
                Some(zaino_json_listen_address),
                None,
            )
        } else {
            (None, None, None, None, None)
        };
        // Wallet lightclients are built by the separate wallet-tests workspace,
        // not here — zaino-testutils carries no zingolib. Tests that need a
        // faucet/recipient launch with `enable_clients: false` and build the
        // clients themselves from `TestManager`'s gRPC address.
        if enable_clients {
            return Err(std::io::Error::other(
                "enable_clients is unsupported in zaino-testutils: build lightclients in the \
                 wallet-tests workspace from TestManager's gRPC address instead.",
            ));
        }
        let test_manager = Self {
            local_net,
            data_dir,
            network: network_kind,
            full_node_rpc_listen_address,
            full_node_grpc_listen_address: validator_settings
                .validator_grpc_listen_address
                .as_ref()
                .and_then(|addr| addr.parse().ok())
                .unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)),
            zaino_handle,
            zaino_json_rpc_listen_address: zaino_json_listen_address,
            zaino_grpc_listen_address,
            service_subscriber: zaino_service_subscriber,
            json_server_cookie_dir: zaino_json_server_cookie_dir,
        };

        test_manager.activate_nu5_nu6(enable_zaino).await;
        Ok(test_manager)
    }

    /// Waits for zaino to be ready, then generates a block to activate NU5/NU6.
    ///
    /// Must be called after construction so the indexer subscriber is live
    /// before the first block-and-wait round-trip.
    async fn activate_nu5_nu6(&self, enable_zaino: bool) {
        // Generate an extra block to turn on NU5 and NU6,
        // as they currently must be turned on at block height = 2.
        match (enable_zaino, self.service_subscriber.as_ref()) {
            (true, Some(subscriber)) => {
                debug!("[TEST] Waiting for Zaino to be ready");
                poll_until_ready(
                    subscriber,
                    std::time::Duration::from_millis(100),
                    std::time::Duration::from_secs(30),
                )
                .await;
                self.generate_blocks_and_wait_for_tip(1, subscriber).await;
            }
            _ => {
                self.local_net.generate_blocks(1).await.unwrap();
            }
        }

        debug!("[TEST] Test environment ready");
    }

    /// Generate `n` blocks and wait for `pollable` to observe each new tip.
    ///
    /// Anything implementing [`PollableTip`] (a Zaino service subscriber, a
    /// `NodeBackedChainIndexSubscriber`, or any future pollable) can be
    /// passed. Fails fast (panics) if `pollable` reports non-live status.
    pub async fn generate_blocks_and_wait_for_tip<P: PollableTip>(&self, n: u32, pollable: &P) {
        fn assert_live<S: Status>(pollable: &S, waiting_for: Option<u64>) {
            if !pollable.is_live() {
                let status = pollable.status();
                match waiting_for {
                    Some(h) => panic!(
                        "Pollable is not live while waiting for block {h} (status: {status:?})."
                    ),
                    None => panic!(
                        "Pollable is not live (status: {status:?}). \
                         The backing validator may have crashed or become unreachable."
                    ),
                }
            }
        }

        let chain_height = u64::from(self.local_net.get_chain_height().await);
        let target_height = chain_height + n as u64;
        let mut next_block_height = chain_height + 1;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(50));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;

        // NOTE: readstate service seems to not be functioning correctly when generate multiple blocks at once and polling the latest block.
        // commented out a fall back to `get_block` to query the cache directly if needed in the future.
        // while indexer.get_block(zaino_proto::proto::service::BlockId {
        //     height: target_height,
        //     hash: vec![],
        // }).await.is_err()
        while pollable.tip_height().await < target_height {
            assert_live(pollable, None);
            if n == 0 {
                interval.tick().await;
            } else {
                self.local_net.generate_blocks(1).await.unwrap();
                while pollable.tip_height().await != next_block_height {
                    assert_live(pollable, Some(next_block_height));
                    interval.tick().await;
                }
                next_block_height += 1;
            }
        }

        // After height is reached, wait for readiness and measure if it adds time
        if !pollable.is_ready() {
            let start = std::time::Instant::now();
            poll_until_ready(
                pollable,
                std::time::Duration::from_millis(50),
                std::time::Duration::from_secs(30),
            )
            .await;
            let elapsed = start.elapsed();
            if elapsed.as_millis() > 0 {
                info!(
                    "Readiness wait after height poll took {:?} (height polling alone was insufficient)",
                    elapsed
                );
            }
        }
    }

    /// Generate `n` blocks and wait for two chain-index subscribers to observe
    /// the resulting tip. Blocks are mined while waiting on `mined_against`;
    /// `then_synced` is afterwards polled (with no further block generation)
    /// until it catches up to that same tip.
    ///
    /// For tests that maintain two independent chain indexes (e.g. a fetch and
    /// a state backend, or a Zaino index and a validator-direct index) and must
    /// see both in sync before querying them.
    pub async fn generate_blocks_and_wait_for_tips<A: PollableTip, B: PollableTip>(
        &self,
        n: u32,
        mined_against: &A,
        then_synced: &B,
    ) {
        self.generate_blocks_and_wait_for_tip(n, mined_against)
            .await;
        self.generate_blocks_and_wait_for_tip(0, then_synced).await;
    }

    /// Build a JSON-RPC connector to the backing validator's RPC port, using
    /// the regtest test cookie credentials. For tests that compare Zaino's
    /// output against the validator's own JSON-RPC.
    pub async fn full_node_jsonrpc_connector(&self) -> JsonRpSeeConnector {
        JsonRpSeeConnector::new_with_basic_auth(
            test_node_and_return_url(
                &self.full_node_rpc_listen_address.to_string(),
                None,
                Some("xxxxxx".to_string()),
                Some("xxxxxx".to_string()),
            )
            .await
            .unwrap(),
            "xxxxxx".to_string(),
            "xxxxxx".to_string(),
        )
        .unwrap()
    }

    /// Closes the TestManager.
    pub async fn close(&mut self) {
        if let Some(handle) = self.zaino_handle.take() {
            handle.abort();
        }
    }
}

/// Launch a state-backend [`TestManager`] alongside standalone [`FetchService`]
/// and [`StateService`] instances pointed at the same validator, returning each
/// service and its subscriber.
///
/// This is the shared core of the `create_test_manager_and_services` test
/// harness used by both the walletless and wallet integration-test workspaces.
/// Wallet callers wrap this and additionally build lightclients from the
/// returned manager's gRPC address.
#[allow(deprecated)]
pub async fn launch_state_and_fetch_services<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<PathBuf>,
    enable_zaino: bool,
    network: Option<NetworkKind>,
) -> (
    TestManager<V, StateService>,
    FetchService,
    FetchServiceSubscriber,
    StateService,
    StateServiceSubscriber,
) {
    let test_manager = TestManager::<V, StateService>::launch(
        validator,
        network,
        None,
        chain_cache.clone(),
        enable_zaino,
        false,
        false,
    )
    .await
    .unwrap();

    let network_type = match network {
        Some(NetworkKind::Mainnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            Network::Mainnet
        }
        Some(NetworkKind::Testnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            Network::Testnet
        }
        _ => Network::Regtest({
            let activation_heights = test_manager.local_net.get_activation_heights().await;
            ActivationHeights {
                before_overwinter: activation_heights.overwinter(),
                overwinter: activation_heights.overwinter(),
                sapling: activation_heights.sapling(),
                blossom: activation_heights.blossom(),
                heartwood: activation_heights.heartwood(),
                canopy: activation_heights.canopy(),
                nu5: activation_heights.nu5(),
                nu6: activation_heights.nu6(),
                nu6_1: activation_heights.nu6_1(),
                nu6_2: activation_heights.nu6_2(),
                nu7: activation_heights.nu7(),
            }
        }),
    };

    test_manager.local_net.print_stdout();

    let fetch_service = spawn_fetch_service(
        test_manager.full_node_rpc_listen_address.to_string(),
        None,
        test_manager
            .local_net
            .data_dir()
            .path()
            .join("fetch-service-zaino"),
        network_type,
    )
    .await;

    let fetch_subscriber = fetch_service.get_subscriber().inner();

    let state_chain_cache_dir = match chain_cache {
        Some(dir) => dir,
        None => test_manager.data_dir.clone(),
    };

    let state_service = StateService::spawn(StateServiceConfig::new(
        zebra_state::Config {
            cache_dir: state_chain_cache_dir,
            ephemeral: false,
            delete_old_database: true,
            debug_stop_at_height: None,
            debug_validity_check_interval: None,
            should_backup_non_finalized_state: false,
            debug_skip_non_finalized_state_backup_task: false,
        },
        test_manager.full_node_rpc_listen_address.to_string(),
        test_manager.full_node_grpc_listen_address,
        false,
        None,
        None,
        None,
        ServiceConfig::default(),
        StorageConfig {
            database: DatabaseConfig {
                path: test_manager
                    .local_net
                    .data_dir()
                    .path()
                    .to_path_buf()
                    .join("state-srvice-zaino"),
                ..Default::default()
            },
            ..Default::default()
        },
        network_type,
        None,
    ))
    .await
    .unwrap();

    let state_subscriber = state_service.get_subscriber().inner();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    (
        test_manager,
        fetch_service,
        fetch_subscriber,
        state_service,
        state_subscriber,
    )
}

/// Spawn a standalone [`FetchService`] pointed at `rpc_url`, storing its index
/// under `db_path`. Shared by the launch helpers below.
#[allow(deprecated)]
async fn spawn_fetch_service(
    rpc_url: String,
    cookie_dir: Option<PathBuf>,
    db_path: PathBuf,
    network: Network,
) -> FetchService {
    FetchService::spawn(FetchServiceConfig::new(
        rpc_url,
        cookie_dir,
        None,
        None,
        ServiceConfig::default(),
        StorageConfig {
            database: DatabaseConfig {
                path: db_path,
                ..Default::default()
            },
            ..Default::default()
        },
        network,
        None,
    ))
    .await
    .unwrap()
}

/// Launch a zcashd [`TestManager`] (with the JSON-RPC server enabled) alongside
/// two standalone [`FetchService`] instances — one pointed at zcashd directly,
/// one at Zaino's JSON-RPC server — for tests that compare the two.
///
/// Shared core of the `create_zcashd_test_manager_and_fetch_services` harness in
/// both integration-test workspaces. Wallet callers wrap this and additionally
/// build lightclients from the returned manager's gRPC address.
#[allow(deprecated)]
pub async fn launch_zcashd_dual_fetch_services() -> (
    TestManager<Zcashd, FetchService>,
    FetchService,
    FetchServiceSubscriber,
    FetchService,
    FetchServiceSubscriber,
) {
    let test_manager = TestManager::<Zcashd, FetchService>::launch(
        &ValidatorKind::Zcashd,
        None,
        None,
        None,
        true,
        true,
        false,
    )
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let zcashd_fetch_service = spawn_fetch_service(
        test_manager.full_node_rpc_listen_address.to_string(),
        None,
        test_manager
            .local_net
            .data_dir()
            .path()
            .join("zcashd-fetch-service-zaino"),
        Network::Regtest(ActivationHeights::default()),
    )
    .await;
    let zcashd_subscriber = zcashd_fetch_service.get_subscriber().inner();

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let zaino_fetch_service = spawn_fetch_service(
        test_manager
            .zaino_json_rpc_listen_address
            .expect("zaino jsonrpc address must be active for these tests")
            .to_string(),
        test_manager.json_server_cookie_dir.clone(),
        test_manager
            .local_net
            .data_dir()
            .path()
            .join("zaino-fetch-service-zaino"),
        Network::Regtest(ActivationHeights::default()),
    )
    .await;
    let zaino_subscriber = zaino_fetch_service.get_subscriber().inner();

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    (
        test_manager,
        zcashd_fetch_service,
        zcashd_subscriber,
        zaino_fetch_service,
        zaino_subscriber,
    )
}

/// Return a copy of `info` with its final (timestamp) field zeroed, so two
/// `getinfo` responses from different sources can be compared without spurious
/// timestamp differences.
pub fn get_info_with_zeroed_timestamp(info: GetInfo) -> GetInfo {
    let (
        version,
        build,
        subversion,
        protocol_version,
        blocks,
        connections,
        proxy,
        difficulty,
        testnet,
        pay_tx_fee,
        relay_fee,
        errors,
        _,
    ) = info.into_parts();
    GetInfo::new(
        version,
        build,
        subversion,
        protocol_version,
        blocks,
        connections,
        proxy,
        difficulty,
        testnet,
        pay_tx_fee,
        relay_fee,
        errors,
        0,
    )
}

/// Launch a fetch-backend [`TestManager`] and return it together with its own
/// service subscriber — the shared core of the `create_test_manager_and_fetch_service`
/// harness in both integration-test workspaces. Wallet callers wrap this and
/// build lightclients from the returned manager's gRPC address.
#[allow(deprecated)]
pub async fn launch_with_fetch_subscriber<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<PathBuf>,
) -> (TestManager<V, FetchService>, FetchServiceSubscriber) {
    let mut test_manager = TestManager::<V, FetchService>::launch(
        validator,
        None,
        None,
        chain_cache,
        true,
        false,
        false,
    )
    .await
    .unwrap();
    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();
    (test_manager, fetch_service_subscriber)
}

impl<C: Validator, Service: LightWalletService + Send + Sync + 'static> Drop
    for TestManager<C, Service>
{
    fn drop(&mut self) {
        debug!("[TEST] Shutting down test environment");
        if let Some(handle) = &self.zaino_handle {
            debug!("[TEST] Aborting Zaino handle");
            handle.abort();
        };
        debug!("[TEST] Test environment shutdown complete");
    }
}

/// Builds a client for creating RPC requests to the indexer/light-node
#[cfg(test)]
async fn build_client(
    uri: http::Uri,
) -> Result<CompactTxStreamerClient<Channel>, tonic::transport::Error> {
    CompactTxStreamerClient::connect(uri).await
}

#[cfg(test)]
mod launch_testmanager {
    use super::*;
    #[allow(deprecated)]
    use zaino_state::FetchService;

    mod zcashd {
        use zcash_local_net::validator::zcashd::Zcashd;

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn basic() {
            let mut test_manager = TestManager::<Zcashd, FetchService>::launch(
                &ValidatorKind::Zcashd,
                None,
                None,
                None,
                false,
                false,
                false,
            )
            .await
            .unwrap();
            assert_eq!(2, (test_manager.local_net.get_chain_height().await));
            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn generate_blocks() {
            let mut test_manager = TestManager::<Zcashd, FetchService>::launch(
                &ValidatorKind::Zcashd,
                None,
                None,
                None,
                false,
                false,
                false,
            )
            .await
            .unwrap();
            assert_eq!(2, (test_manager.local_net.get_chain_height().await));
            test_manager.local_net.generate_blocks(1).await.unwrap();
            assert_eq!(3, (test_manager.local_net.get_chain_height().await));
            test_manager.close().await;
        }

        #[ignore = "chain cache needs development"]
        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn with_chain() {
            let mut test_manager = TestManager::<Zcashd, FetchService>::launch(
                &ValidatorKind::Zcashd,
                None,
                None,
                ZCASHD_CHAIN_CACHE_DIR.clone(),
                false,
                false,
                false,
            )
            .await
            .unwrap();
            assert_eq!(10, (test_manager.local_net.get_chain_height().await));
            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn zaino() {
            let mut test_manager = TestManager::<Zcashd, FetchService>::launch(
                &ValidatorKind::Zcashd,
                None,
                None,
                None,
                true,
                false,
                false,
            )
            .await
            .unwrap();

            let _grpc_client = build_client(test_manager.grpc_socket_to_uri())
                .await
                .unwrap();
            test_manager.close().await;
        }
    }

    mod zebrad {
        use super::*;

        mod fetch_service {

            use zcash_local_net::validator::zebrad::Zebrad;

            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn basic() {
                let mut test_manager = TestManager::<Zebrad, FetchService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    false,
                    false,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(2, (test_manager.local_net.get_chain_height().await));
                test_manager.close().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn generate_blocks() {
                let mut test_manager = TestManager::<Zebrad, FetchService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    false,
                    false,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(2, (test_manager.local_net.get_chain_height().await));
                test_manager.local_net.generate_blocks(1).await.unwrap();
                assert_eq!(3, (test_manager.local_net.get_chain_height().await));
                test_manager.close().await;
            }

            #[ignore = "chain cache needs development"]
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn with_chain() {
                let mut test_manager = TestManager::<Zebrad, FetchService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    ZEBRAD_CHAIN_CACHE_DIR.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(52, (test_manager.local_net.get_chain_height().await));
                test_manager.close().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino() {
                let mut test_manager = TestManager::<Zebrad, FetchService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    true,
                    false,
                    false,
                )
                .await
                .unwrap();
                let _grpc_client = build_client(test_manager.grpc_socket_to_uri())
                    .await
                    .unwrap();
                test_manager.close().await;
            }
        }

        mod state_service {
            use super::*;
            #[allow(deprecated)]
            use zaino_state::StateService;

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn basic() {
                let mut test_manager = TestManager::<Zebrad, StateService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    false,
                    false,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(2, (test_manager.local_net.get_chain_height().await));
                test_manager.close().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn generate_blocks() {
                let mut test_manager = TestManager::<Zebrad, StateService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    false,
                    false,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(2, (test_manager.local_net.get_chain_height().await));
                test_manager.local_net.generate_blocks(1).await.unwrap();
                assert_eq!(3, (test_manager.local_net.get_chain_height().await));
                test_manager.close().await;
            }

            #[ignore = "chain cache needs development"]
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn with_chain() {
                let mut test_manager = TestManager::<Zebrad, StateService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    ZEBRAD_CHAIN_CACHE_DIR.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(52, (test_manager.local_net.get_chain_height().await));
                test_manager.close().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino() {
                let mut test_manager = TestManager::<Zebrad, StateService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    true,
                    false,
                    false,
                )
                .await
                .unwrap();
                let _grpc_client = build_client(test_manager.grpc_socket_to_uri())
                    .await
                    .unwrap();
                test_manager.close().await;
            }
        }
    }
}
