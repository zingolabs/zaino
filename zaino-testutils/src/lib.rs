//! Zaino Testing Utilities.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

/// Convenience reexport of zaino_testvectors
pub mod test_vectors {
    pub use zaino_testvectors::*;
}

use once_cell::sync::Lazy;
use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};
use tracing::info;
use tracing_subscriber::EnvFilter;
use zaino_common::{
    network::{ActivationHeights, ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS},
    validator::ValidatorConfig,
    CacheConfig, DatabaseConfig, Network, ServiceConfig, StorageConfig,
};
use zaino_serve::server::config::{GrpcServerConfig, JsonRpcServerConfig};
use zaino_state::{
    chain_index::NonFinalizedSnapshot, BackendType, ChainIndex, LightWalletIndexer,
    LightWalletService, NodeBackedChainIndexSubscriber, ZcashIndexer, ZcashService,
};
use zainodlib::{config::ZainodConfig, error::IndexerError, indexer::Indexer};
pub use zcash_local_net as services;
use zcash_local_net::validator::zebrad::{Zebrad, ZebradConfig};
pub use zcash_local_net::validator::Validator;
use zcash_local_net::validator::ValidatorConfig as _;
use zcash_local_net::{
    error::LaunchError,
    validator::zcashd::{Zcashd, ZcashdConfig},
};
use zcash_local_net::{logs::LogsToStdoutAndStderr, process::Process};
use zcash_protocol::PoolType;
use zebra_chain::parameters::{testnet::ConfiguredActivationHeights, NetworkKind};
use zingo_netutils::{GetClientError, GrpcConnector, UnderlyingService};
use zingo_test_vectors::seeds;
pub use zingolib::get_base_address_macro;
pub use zingolib::lightclient::LightClient;
pub use zingolib::testutils::lightclient::from_inputs;
use zingolib_testutils::scenarios::ClientBuilder;

use zcash_client_backend::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient, ChainSpec,
};

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

#[derive(PartialEq, Clone, Copy)]
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
            validator_jsonrpc_listen_address: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                zebrad.rpc_listen_port(),
            ),
            validator_grpc_listen_address: Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                zebrad.indexer_listen_port(),
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
            validator_jsonrpc_listen_address: SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                zcashd.port(),
            ),
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
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: From<ZainodConfig>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
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
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
            .with_target(true)
            .try_init();

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

        let (local_net, validator_settings) = C::launch_validator_and_return_config(config)
            .await
            .expect("to launch a default validator");
        let rpc_listen_port = local_net.get_port();
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
            info!("{:?}", validator_settings.validator_grpc_listen_address);
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
                validator_settings: dbg!(validator_settings.clone()),
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
            };

            let (handle, service_subscriber) = Indexer::<Service>::launch_inner(
                Service::Config::from(indexer_config.clone()),
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
            let configured_activation_heights = ConfiguredActivationHeights {
                before_overwinter: activation_heights.before_overwinter,
                overwinter: activation_heights.overwinter,
                sapling: activation_heights.sapling,
                blossom: activation_heights.blossom,
                heartwood: activation_heights.heartwood,
                canopy: activation_heights.canopy,
                nu5: activation_heights.nu5,
                nu6: activation_heights.nu6,
                nu6_1: activation_heights.nu6_1,
                nu7: activation_heights.nu7,
            };
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
                .unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)),
            zaino_handle,
            zaino_json_rpc_listen_address: zaino_json_listen_address,
            zaino_grpc_listen_address,
            service_subscriber: zaino_service_subscriber,
            json_server_cookie_dir: zaino_json_server_cookie_dir,
            clients,
        };

        // Generate an extra block to turn on NU5 and NU6,
        // as they currently must be turned on at block height = 2.
        // NOTE: if this is removed when zebra fixes this issue we must replace with a generate_block_and_poll(0) when
        // zaino is enabled to ensure its ready and not still syncing
        if enable_zaino {
            test_manager.generate_blocks_and_poll(1).await;
        } else {
            test_manager.local_net.generate_blocks(1).await.unwrap();
        }

        // FIXME: zaino's status can still be syncing instead of ready at this point
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        Ok(test_manager)
    }

    /// Generate `n` blocks for the local network and poll zaino via gRPC until the chain index is synced to the target height.
    pub async fn generate_blocks_and_poll(&self, n: u32) {
        let mut grpc_client = build_client(services::network::localhost_uri(
            self.zaino_grpc_listen_address
                .expect("Zaino listen port is not available but zaino is active.")
                .port(),
        ))
        .await
        .unwrap();
        let chain_height = self.local_net.get_chain_height().await;
        let mut next_block_height = u64::from(chain_height) + 1;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        while grpc_client
            .get_latest_block(tonic::Request::new(ChainSpec {}))
            .await
            .unwrap()
            .into_inner()
            .height
            < u64::from(chain_height) + n as u64
        {
            if n == 0 {
                interval.tick().await;
            } else {
                self.local_net.generate_blocks(1).await.unwrap();
                while grpc_client
                    .get_latest_block(tonic::Request::new(ChainSpec {}))
                    .await
                    .unwrap()
                    .into_inner()
                    .height
                    != next_block_height
                {
                    interval.tick().await;
                }
                next_block_height += 1;
            }
        }
    }

    /// Generate `n` blocks for the local network and poll zaino's fetch/state subscriber until the chain index is synced to the target height.
    pub async fn generate_blocks_and_poll_indexer(
        &self,
        n: u32,
        indexer: &impl LightWalletIndexer,
    ) {
        let chain_height = self.local_net.get_chain_height().await;
        let mut next_block_height = u64::from(chain_height) + 1;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        // NOTE: readstate service seems to not be functioning correctly when generate multiple blocks at once and polling the latest block.
        // commented out a fall back to `get_block` to query the cache directly if needed in the future.
        // while indexer.get_block(zaino_proto::proto::service::BlockId {
        //     height: u64::from(chain_height) + n as u64,
        //     hash: vec![],
        // }).await.is_err()
        while indexer.get_latest_block().await.unwrap().height < u64::from(chain_height) + n as u64
        {
            if n == 0 {
                interval.tick().await;
            } else {
                self.local_net.generate_blocks(1).await.unwrap();
                while indexer.get_latest_block().await.unwrap().height != next_block_height {
                    interval.tick().await;
                }
                next_block_height += 1;
            }
        }
    }

    /// Generate `n` blocks for the local network and poll zaino's chain index until the chain index is synced to the target height.
    pub async fn generate_blocks_and_poll_chain_index(
        &self,
        n: u32,
        chain_index: &NodeBackedChainIndexSubscriber,
    ) {
        let chain_height = self.local_net.get_chain_height().await;
        let mut next_block_height = chain_height + 1;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        while u32::from(
            chain_index
                .snapshot_nonfinalized_state()
                .best_chaintip()
                .height,
        ) < chain_height + n
        {
            if n == 0 {
                interval.tick().await;
            } else {
                self.local_net.generate_blocks(1).await.unwrap();
                while u32::from(
                    chain_index
                        .snapshot_nonfinalized_state()
                        .best_chaintip()
                        .height,
                ) != next_block_height
                {
                    interval.tick().await;
                }
                next_block_height += 1;
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
        if let Some(handle) = &self.zaino_handle {
            handle.abort();
        };
    }
}

/// Builds a client for creating RPC requests to the indexer/light-node
async fn build_client(
    uri: http::Uri,
) -> Result<CompactTxStreamerClient<UnderlyingService>, GetClientError> {
    GrpcConnector::new(uri).get_client().await
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
            let _grpc_client = build_client(services::network::localhost_uri(
                test_manager
                    .zaino_grpc_listen_address
                    .expect("Zaino listen port is not available but zaino is active.")
                    .port(),
            ))
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
                let _grpc_client = build_client(services::network::localhost_uri(
                    test_manager
                        .zaino_grpc_listen_address
                        .expect("Zaino listen port not available but zaino is active.")
                        .port(),
                ))
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

                test_manager.generate_blocks_and_poll(100).await;
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

                test_manager.generate_blocks_and_poll(100).await;
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
                test_manager.generate_blocks_and_poll(1).await;
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

                test_manager.generate_blocks_and_poll(1).await;
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
                let _grpc_client = build_client(services::network::localhost_uri(
                    test_manager
                        .zaino_grpc_listen_address
                        .expect("Zaino listen port not available but zaino is active.")
                        .port(),
                ))
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

                test_manager.generate_blocks_and_poll(100).await;
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

                test_manager.generate_blocks_and_poll(100).await;
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
                test_manager.generate_blocks_and_poll(1).await;
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

                test_manager.generate_blocks_and_poll(1).await;
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
