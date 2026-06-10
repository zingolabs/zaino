//! Zaino Testing Utilities.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

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
use zaino_serve::server::config::{GrpcServerConfig, JsonRpcServerConfig};
use zaino_state::{
    BackendType, BlockchainSource, ChainIndex, FetchServiceSubscriber, LightWalletIndexer,
    LightWalletService, NodeBackedChainIndexSubscriber, StateServiceSubscriber, ZcashIndexer,
    ZcashService,
};
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
use zebra_chain::parameters::{testnet::ConfiguredActivationHeights, NetworkKind};
#[cfg(test)]
use zingo_netutils::{GetClientError, GrpcIndexer};
use zingo_test_vectors::seeds;
pub use zingolib::get_base_address_macro;
pub use zingolib::lightclient::LightClient;
pub use zingolib::testutils::lightclient::from_inputs;
use zingolib_testutils::scenarios::ClientBuilder;

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

/// Holds zingo lightclients along with the lightclient builder for wallet-2-validator tests.
pub struct Clients {
    /// Lightclient builder.
    pub client_builder: ClientBuilder,
    /// Faucet (zingolib lightclient).
    ///
    /// Mining rewards are received by this client for use in tests.
    pub faucet: zingolib::lightclient::LightClient,
    /// Recipient (zingolib lightclient).
    pub recipient: zingolib::lightclient::LightClient,
}

impl Clients {
    /// Returns the zcash address of the faucet.
    pub async fn get_faucet_address(&self, pool: &str) -> String {
        zingolib::get_base_address_macro!(self.faucet, pool)
    }

    /// Returns the zcash address of the recipient.
    pub async fn get_recipient_address(&self, pool: &str) -> String {
        zingolib::get_base_address_macro!(self.recipient, pool)
    }
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
    /// Zingolib lightclients.
    pub clients: Option<Clients>,
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
    /// If clients is set to active zingolib lightclients will be created for test use.
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
        // Launch Zingolib Lightclients:
        let clients = if enable_clients {
            let mut client_builder = ClientBuilder::new(
                make_uri(
                    zaino_grpc_listen_address
                        .expect("Error launching zingo lightclients. `enable_zaino` is None.")
                        .port(),
                ),
                tempfile::tempdir().unwrap(),
            );

            let configured_activation_heights: ConfiguredActivationHeights =
                activation_heights.into();
            let faucet = client_builder.build_faucet(true, configured_activation_heights);
            let recipient = client_builder.build_client(
                seeds::HOSPITAL_MUSEUM_SEED.to_string(),
                1,
                true,
                configured_activation_heights,
            );
            Some(Clients {
                client_builder,
                faucet,
                recipient,
            })
        } else {
            None
        };
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
            clients,
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

    /// Closes the TestManager.
    pub async fn close(&mut self) {
        if let Some(handle) = self.zaino_handle.take() {
            handle.abort();
        }
    }
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
) -> Result<zingo_netutils::lightwallet_protocol::CompactTxStreamerClient<Channel>, GetClientError>
{
    Ok(GrpcIndexer::new(uri).await?.get_clear_net_client().await)
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

        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn zaino_clients() {
            let mut test_manager = TestManager::<Zcashd, FetchService>::launch(
                &ValidatorKind::Zcashd,
                None,
                None,
                None,
                true,
                false,
                true,
            )
            .await
            .unwrap();
            let clients = test_manager
                .clients
                .as_ref()
                .expect("Clients are not initialized");
            dbg!(clients.faucet.do_info().await);
            dbg!(clients.recipient.do_info().await);
            test_manager.close().await;
        }

        /// This test shows nothing about zebrad.
        /// This is not the case with Zcashd and should not be the case here.
        /// Even if rewards need 100 confirmations these blocks should not have to be mined at the same time.
        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        pub(crate) async fn zaino_clients_receive_mining_reward() {
            let mut test_manager = TestManager::<Zcashd, FetchService>::launch(
                &ValidatorKind::Zcashd,
                None,
                None,
                None,
                true,
                false,
                true,
            )
            .await
            .unwrap();
            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");

            clients.faucet.sync_and_await().await.unwrap();
            dbg!(clients
                .faucet
                .account_balance(zip32::AccountId::ZERO)
                .await
                .unwrap());

            assert!(
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64() > 0
                        || clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().confirmed_transparent_balance.unwrap().into_u64() > 0,
                    "No mining reward received from Zcashd. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64(),
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().confirmed_transparent_balance.unwrap().into_u64()
                );

            test_manager.close().await;
        }
    }

    mod zebrad {
        use super::*;

        mod fetch_service {

            use zcash_local_net::validator::zebrad::Zebrad;
            use zip32::AccountId;

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

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino_clients() {
                let mut test_manager = TestManager::<Zebrad, FetchService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    true,
                    false,
                    true,
                )
                .await
                .unwrap();
                let clients = test_manager
                    .clients
                    .as_ref()
                    .expect("Clients are not initialized");
                dbg!(clients.faucet.do_info().await);
                dbg!(clients.recipient.do_info().await);
                test_manager.close().await;
            }

            /// This test shows currently we do not receive mining rewards from Zebra unless we mine 100 blocks at a time.
            /// This is not the case with Zcashd and should not be the case here.
            /// Even if rewards need 100 confirmations these blocks should not have to be mined at the same time.
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino_clients_receive_mining_reward() {
                let mut test_manager = TestManager::<Zebrad, FetchService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    true,
                    false,
                    true,
                )
                .await
                .unwrap();
                let mut clients = test_manager
                    .clients
                    .take()
                    .expect("Clients are not initialized");

                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients
                    .faucet
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                test_manager
                    .generate_blocks_and_wait_for_tip(100, test_manager.subscriber())
                    .await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients
                    .faucet
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                assert!(
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64() > 0
                        || clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().confirmed_transparent_balance.unwrap().into_u64() > 0,
                    "No mining reward received from Zebrad. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64(),
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().confirmed_transparent_balance.unwrap().into_u64()
            );

                test_manager.close().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino_clients_receive_mining_reward_and_send() {
                let mut test_manager = TestManager::<Zebrad, FetchService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    true,
                    false,
                    true,
                )
                .await
                .unwrap();
                let mut clients = test_manager
                    .clients
                    .take()
                    .expect("Clients are not initialized");

                test_manager
                    .generate_blocks_and_wait_for_tip(100, test_manager.subscriber())
                    .await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients
                    .faucet
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                assert!(
                    clients
                        .faucet
                        .account_balance(zip32::AccountId::ZERO)
                        .await
                        .unwrap()
                        .confirmed_transparent_balance
                        .unwrap()
                        .into_u64()
                        > 0,
                    "No mining reward received from Zebrad. Faucet Transparent Balance: {:}.",
                    clients
                        .faucet
                        .account_balance(zip32::AccountId::ZERO)
                        .await
                        .unwrap()
                        .confirmed_transparent_balance
                        .unwrap()
                        .into_u64()
                );

                // *Send all transparent funds to own orchard address.
                clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
                test_manager
                    .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
                    .await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients
                    .faucet
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                assert!(
                clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64() > 0,
                "No funds received from shield. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64(),
                clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().confirmed_transparent_balance.unwrap().into_u64()
            );

                let recipient_zaddr = clients.get_recipient_address("sapling").await.to_string();
                zingolib::testutils::lightclient::from_inputs::quick_send(
                    &mut clients.faucet,
                    vec![(&recipient_zaddr, 250_000, None)],
                )
                .await
                .unwrap();

                test_manager
                    .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                clients.recipient.sync_and_await().await.unwrap();
                dbg!(clients
                    .recipient
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                assert_eq!(
                    clients
                        .recipient
                        .account_balance(zip32::AccountId::ZERO)
                        .await
                        .unwrap()
                        .confirmed_sapling_balance
                        .unwrap()
                        .into_u64(),
                    250_000
                );

                test_manager.close().await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino_testnet() {
                let mut test_manager = TestManager::<Zebrad, FetchService>::launch(
                    &ValidatorKind::Zebrad,
                    Some(NetworkKind::Testnet),
                    None,
                    ZEBRAD_TESTNET_CACHE_DIR.clone(),
                    true,
                    false,
                    true,
                )
                .await
                .unwrap();
                let clients = test_manager
                    .clients
                    .as_ref()
                    .expect("Clients are not initialized");
                dbg!(clients.faucet.do_info().await);
                dbg!(clients.recipient.do_info().await);
                test_manager.close().await;
            }
        }

        mod state_service {
            use super::*;
            #[allow(deprecated)]
            use zaino_state::StateService;
            use zip32::AccountId;

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

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino_clients() {
                let mut test_manager = TestManager::<Zebrad, StateService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    true,
                    false,
                    true,
                )
                .await
                .unwrap();
                let clients = test_manager
                    .clients
                    .as_ref()
                    .expect("Clients are not initialized");
                dbg!(clients.faucet.do_info().await);
                dbg!(clients.recipient.do_info().await);
                test_manager.close().await;
            }

            /// This test shows currently we do not receive mining rewards from Zebra unless we mine 100 blocks at a time.
            /// This is not the case with Zcashd and should not be the case here.
            /// Even if rewards need 100 confirmations these blocks should not have to be mined at the same time.
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino_clients_receive_mining_reward() {
                let mut test_manager = TestManager::<Zebrad, StateService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    true,
                    false,
                    true,
                )
                .await
                .unwrap();

                let mut clients = test_manager
                    .clients
                    .take()
                    .expect("Clients are not initialized");

                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients
                    .faucet
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                test_manager
                    .generate_blocks_and_wait_for_tip(100, test_manager.subscriber())
                    .await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients
                    .faucet
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                assert!(
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64() > 0
                        || clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().confirmed_transparent_balance.unwrap().into_u64() > 0,
                    "No mining reward received from Zebrad. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64(),
                    clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().confirmed_transparent_balance.unwrap().into_u64()
            );

                test_manager.close().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino_clients_receive_mining_reward_and_send() {
                let mut test_manager = TestManager::<Zebrad, StateService>::launch(
                    &ValidatorKind::Zebrad,
                    None,
                    None,
                    None,
                    true,
                    false,
                    true,
                )
                .await
                .unwrap();

                let mut clients = test_manager
                    .clients
                    .take()
                    .expect("Clients are not initialized");

                test_manager
                    .generate_blocks_and_wait_for_tip(100, test_manager.subscriber())
                    .await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients
                    .faucet
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                assert!(
                    clients
                        .faucet
                        .account_balance(zip32::AccountId::ZERO)
                        .await
                        .unwrap()
                        .confirmed_transparent_balance
                        .unwrap()
                        .into_u64()
                        > 0,
                    "No mining reward received from Zebrad. Faucet Transparent Balance: {:}.",
                    clients
                        .faucet
                        .account_balance(zip32::AccountId::ZERO)
                        .await
                        .unwrap()
                        .confirmed_transparent_balance
                        .unwrap()
                        .into_u64()
                );

                // *Send all transparent funds to own orchard address.
                clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
                test_manager
                    .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
                    .await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients
                    .faucet
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                assert!(
                clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64() > 0,
                "No funds received from shield. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().total_orchard_balance.unwrap().into_u64(),
                clients.faucet.account_balance(zip32::AccountId::ZERO).await.unwrap().confirmed_transparent_balance.unwrap().into_u64()
            );

                let recipient_zaddr = clients.get_recipient_address("sapling").await.to_string();
                zingolib::testutils::lightclient::from_inputs::quick_send(
                    &mut clients.faucet,
                    vec![(&recipient_zaddr, 250_000, None)],
                )
                .await
                .unwrap();

                test_manager
                    .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
                    .await;
                clients.recipient.sync_and_await().await.unwrap();
                dbg!(clients
                    .recipient
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .unwrap());

                assert_eq!(
                    clients
                        .recipient
                        .account_balance(zip32::AccountId::ZERO)
                        .await
                        .unwrap()
                        .confirmed_sapling_balance
                        .unwrap()
                        .into_u64(),
                    250_000
                );

                test_manager.close().await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            #[allow(deprecated)]
            pub(crate) async fn zaino_testnet() {
                let mut test_manager = TestManager::<Zebrad, StateService>::launch(
                    &ValidatorKind::Zebrad,
                    Some(NetworkKind::Testnet),
                    None,
                    ZEBRAD_TESTNET_CACHE_DIR.clone(),
                    true,
                    false,
                    true,
                )
                .await
                .unwrap();
                let clients = test_manager
                    .clients
                    .as_ref()
                    .expect("Clients are not initialized");
                dbg!(clients.faucet.do_info().await);
                dbg!(clients.recipient.do_info().await);
                test_manager.close().await;
            }
        }
    }
}
