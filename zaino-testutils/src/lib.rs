//! Zaino Testing Utilities.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

/// Convenience reexport of zaino_testvectors
pub mod test_vectors {
    pub use zaino_testvectors::*;
}

use once_cell::sync::Lazy;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};
use tempfile::TempDir;
use testvectors::seeds;
use tracing_subscriber::EnvFilter;
use zaino_common::{
    network::ActivationHeights, CacheConfig, DatabaseConfig, Network, ServiceConfig, StorageConfig,
};
use zaino_state::BackendType;
use zainodlib::config::default_ephemeral_cookie_path;
use zebra_chain::parameters::NetworkKind;
pub use zingo_infra_services as services;
pub use zingo_infra_services::validator::Validator;
use zingo_infra_services::validator::{ZcashdConfig, ZebradConfig};
pub use zingolib::get_base_address_macro;
pub use zingolib::lightclient::LightClient;
pub use zingolib::testutils::lightclient::from_inputs;
use zingolib::testutils::scenarios::ClientBuilder;

/// Helper to get the test binary path from the TEST_BINARIES_DIR env var.
fn binary_path(binary_name: &str) -> Option<PathBuf> {
    std::env::var("TEST_BINARIES_DIR")
        .ok()
        .map(|dir| PathBuf::from(dir).join(binary_name))
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
pub enum ValidatorConfig {
    /// Zcashd Config.
    ZcashdConfig(ZcashdConfig),
    /// Zebrad Config.
    ZebradConfig(zingo_infra_services::validator::ZebradConfig),
}

/// Available zcash-local-net configurations.
#[allow(
    clippy::large_enum_variant,
    reason = "Maybe this issue: https://github.com/rust-lang/rust-clippy/issues/9798"
)]
pub enum LocalNet {
    /// Zcash-local-net backed by Zcashd.
    Zcashd(
        zingo_infra_services::LocalNet<
            zingo_infra_services::indexer::Empty,
            zingo_infra_services::validator::Zcashd,
        >,
    ),
    /// Zcash-local-net backed by Zebrad.
    Zebrad(
        zingo_infra_services::LocalNet<
            zingo_infra_services::indexer::Empty,
            zingo_infra_services::validator::Zebrad,
        >,
    ),
}

impl zingo_infra_services::validator::Validator for LocalNet {
    const CONFIG_FILENAME: &str = "";
    const PROCESS: zingo_infra_services::Process = zingo_infra_services::Process::Empty; // todo

    type Config = ValidatorConfig;

    fn default_test_config() -> Self::Config {
        todo!()
    }

    fn activation_heights(&self) -> zcash_protocol::local_consensus::LocalNetwork {
        // Return the activation heights for the network
        // This depends on which validator is running (zcashd or zebrad)
        match self {
            LocalNet::Zcashd(net) => net.validator().activation_heights(),
            LocalNet::Zebrad(net) => net.validator().activation_heights(),
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn launch(
        config: Self::Config,
    ) -> impl std::future::Future<Output = Result<Self, zingo_infra_services::error::LaunchError>> + Send
    {
        async move {
            match config {
                ValidatorConfig::ZcashdConfig(cfg) => {
                    let net = zingo_infra_services::LocalNet::<
                        zingo_infra_services::indexer::Empty,
                        zingo_infra_services::validator::Zcashd,
                    >::launch(
                        zingo_infra_services::indexer::EmptyConfig {}, cfg
                    )
                    .await;
                    Ok(LocalNet::Zcashd(net))
                }
                ValidatorConfig::ZebradConfig(cfg) => {
                    let net = zingo_infra_services::LocalNet::<
                        zingo_infra_services::indexer::Empty,
                        zingo_infra_services::validator::Zebrad,
                    >::launch(
                        zingo_infra_services::indexer::EmptyConfig {}, cfg
                    )
                    .await;
                    Ok(LocalNet::Zebrad(net))
                }
            }
        }
    }

    fn stop(&mut self) {
        match self {
            LocalNet::Zcashd(net) => net.validator_mut().stop(),
            LocalNet::Zebrad(net) => net.validator_mut().stop(),
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn generate_blocks(
        &self,
        n: u32,
    ) -> impl std::future::Future<Output = std::io::Result<()>> + Send {
        async move {
            match self {
                LocalNet::Zcashd(net) => net.validator().generate_blocks(n).await,
                LocalNet::Zebrad(net) => net.validator().generate_blocks(n).await,
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn get_chain_height(
        &self,
    ) -> impl std::future::Future<Output = zcash_protocol::consensus::BlockHeight> + Send {
        async move {
            match self {
                LocalNet::Zcashd(net) => net.validator().get_chain_height().await,
                LocalNet::Zebrad(net) => net.validator().get_chain_height().await,
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn poll_chain_height(
        &self,
        target_height: zcash_protocol::consensus::BlockHeight,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            match self {
                LocalNet::Zcashd(net) => net.validator().poll_chain_height(target_height).await,
                LocalNet::Zebrad(net) => net.validator().poll_chain_height(target_height).await,
            }
        }
    }

    fn config_dir(&self) -> &TempDir {
        match self {
            LocalNet::Zcashd(net) => net.validator().config_dir(),
            LocalNet::Zebrad(net) => net.validator().config_dir(),
        }
    }

    fn logs_dir(&self) -> &TempDir {
        match self {
            LocalNet::Zcashd(net) => net.validator().logs_dir(),
            LocalNet::Zebrad(net) => net.validator().logs_dir(),
        }
    }

    fn data_dir(&self) -> &TempDir {
        match self {
            LocalNet::Zcashd(net) => net.validator().data_dir(),
            LocalNet::Zebrad(net) => net.validator().data_dir(),
        }
    }

    fn network(&self) -> NetworkKind {
        match self {
            LocalNet::Zcashd(net) => net.validator().network(),
            LocalNet::Zebrad(net) => *net.validator().network(),
        }
    }

    /// Prints the stdout log.
    fn print_stdout(&self) {
        match self {
            LocalNet::Zcashd(net) => net.validator().print_stdout(),
            LocalNet::Zebrad(net) => net.validator().print_stdout(),
        }
    }

    /// Chain_Cache PathBuf must contain validator bin name for this function to function.
    fn load_chain(
        chain_cache: PathBuf,
        validator_data_dir: PathBuf,
        validator_network: NetworkKind,
    ) -> PathBuf {
        if chain_cache.to_string_lossy().contains("zcashd") {
            zingo_infra_services::validator::Zcashd::load_chain(
                chain_cache,
                validator_data_dir,
                validator_network.into(),
            )
        } else if chain_cache.to_string_lossy().contains("zebrad") {
            zingo_infra_services::validator::Zebrad::load_chain(
                chain_cache,
                validator_data_dir,
                validator_network.into(),
            )
        } else {
            panic!(
                "Invalid chain_cache path: expected to contain 'zcashd' or 'zebrad', but got: {}",
                chain_cache.display()
            );
        }
    }
}

/// Holds zingo lightclients for wallet-2-validator tests.
// used to hold a tempdir, but it was used elsewhere instead
pub struct Clients {
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

/// Configuration data for Zingo-Indexer Tests.
pub struct TestManager {
    /// Zcash-local-net.
    pub local_net: LocalNet,
    /// Data directory for the validator.
    pub data_dir: PathBuf,
    /// Network (chain) type:
    pub network: NetworkKind,
    /// Zebrad/Zcashd JsonRpc listen address.
    pub zebrad_rpc_listen_address: SocketAddr,
    /// Zebrad/Zcashd gRpc listen address.
    pub zebrad_grpc_listen_address: SocketAddr,
    /// Zaino Indexer JoinHandle.
    pub zaino_handle: Option<tokio::task::JoinHandle<Result<(), zainodlib::error::IndexerError>>>,
    /// Zingo-Indexer JsonRPC listen address.
    pub zaino_json_rpc_listen_address: Option<SocketAddr>,
    /// Zingo-Indexer gRPC listen address.
    pub zaino_grpc_listen_address: Option<SocketAddr>,
    /// JsonRPC server cookie dir.
    pub json_server_cookie_dir: Option<PathBuf>,
    /// Zingolib lightclients.
    pub clients: Option<Clients>,
}

fn make_uri(indexer_port: portpicker::Port) -> http::Uri {
    format!("http://127.0.0.1:{indexer_port}")
        .try_into()
        .unwrap()
}
// NOTE: this should be migrated to zingolib when LocalNet replaces regtest manager in zingoilb::testutils
/// Builds faucet (miner) and recipient lightclients for local network integration testing
async fn build_lightclients(
    lightclient_dir: TempDir,
    indexer_port: portpicker::Port,
) -> (LightClient, LightClient) {
    let activation_heights =
        zingo_common_components::protocol::activation_heights::test::block_one();
    let mut client_builder = ClientBuilder::new(make_uri(indexer_port), lightclient_dir);
    let faucet = client_builder.build_faucet(true, activation_heights);
    let recipient = client_builder.build_client(
        seeds::HOSPITAL_MUSEUM_SEED.to_string(),
        1,
        true,
        activation_heights,
    );

    (faucet, recipient)
}
impl TestManager {
    /// Launches zcash-local-net<Empty, Validator>.
    ///
    /// Possible validators: Zcashd, Zebrad.
    ///
    /// If chain_cache is given a path the chain will be loaded.
    ///
    /// If clients is set to active zingolib lightclients will be created for test use.
    ///
    /// TODO: Add TestManagerConfig struct and constructor methods of common test setups.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        validator: &ValidatorKind,
        backend: &BackendType,
        network: Option<NetworkKind>,
        chain_cache: Option<PathBuf>,
        enable_zaino: bool,
        enable_zaino_jsonrpc_server: bool,
        enable_zaino_jsonrpc_server_cookie_auth: bool,
        zaino_no_sync: bool,
        zaino_no_db: bool,
        enable_clients: bool,
    ) -> Result<Self, std::io::Error> {
        if (validator == &ValidatorKind::Zcashd) && (backend == &BackendType::State) {
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

        let activation_heights = ActivationHeights::default();
        let network_kind = network.unwrap_or(NetworkKind::Regtest);
        let zaino_network_kind =
            Network::from_network_kind_and_activation_heights(&network_kind, &activation_heights);

        if enable_clients && !enable_zaino {
            return Err(std::io::Error::other(
                "Cannot enable clients when zaino is not enabled.",
            ));
        }

        // Launch LocalNet:
        let rpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
        let grpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
        let zebrad_rpc_listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_listen_port);
        let zebrad_grpc_listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), grpc_listen_port);

        let validator_config = match validator {
            ValidatorKind::Zcashd => {
                let mut cfg = ZcashdConfig::default_test();
                cfg.rpc_listen_port = Some(rpc_listen_port);
                cfg.chain_cache = chain_cache.clone();
                ValidatorConfig::ZcashdConfig(cfg)
            }
            ValidatorKind::Zebrad => {
                let mut cfg = ZebradConfig::default_test();
                cfg.rpc_listen_port = Some(rpc_listen_port);
                cfg.indexer_listen_port = Some(grpc_listen_port);
                cfg.chain_cache = chain_cache.clone();
                cfg.network = network_kind;
                ValidatorConfig::ZebradConfig(cfg)
            }
        };
        let local_net = LocalNet::launch(validator_config).await.unwrap();
        let data_dir = local_net.data_dir().path().to_path_buf();
        let zaino_db_path = data_dir.join("zaino");

        let zebra_db_path = match chain_cache {
            Some(cache) => cache,
            None => data_dir.clone(),
        };

        // Launch Zaino:
        let (
            zaino_grpc_listen_address,
            zaino_json_listen_address,
            zaino_json_server_cookie_dir,
            zaino_handle,
        ) = if enable_zaino {
            let zaino_grpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
            let zaino_grpc_listen_address =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zaino_grpc_listen_port);

            let zaino_json_listen_port = portpicker::pick_unused_port().expect("No ports free");
            let zaino_json_listen_address =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zaino_json_listen_port);
            let zaino_json_server_cookie_dir = Some(default_ephemeral_cookie_path());

            let indexer_config = zainodlib::config::IndexerConfig {
                // TODO: Make configurable.
                backend: *backend,
                enable_json_server: enable_zaino_jsonrpc_server,
                json_rpc_listen_address: zaino_json_listen_address,
                enable_cookie_auth: enable_zaino_jsonrpc_server_cookie_auth,
                cookie_dir: zaino_json_server_cookie_dir.clone(),
                grpc_listen_address: zaino_grpc_listen_address,
                grpc_tls: false,
                tls_cert_path: None,
                tls_key_path: None,
                validator_listen_address: zebrad_rpc_listen_address,
                validator_grpc_listen_address: zebrad_grpc_listen_address,
                validator_cookie_auth: false,
                validator_cookie_path: None,
                validator_user: Some("xxxxxx".to_string()),
                validator_password: Some("xxxxxx".to_string()),
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
                no_sync: zaino_no_sync,
                no_db: zaino_no_db,
                slow_sync: false,
            };
            let handle = zainodlib::indexer::spawn_indexer(indexer_config)
                .await
                .unwrap();

            // NOTE: This is required to give the server time to launch, this is not used in production code but could be rewritten to improve testing efficiency.
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            (
                Some(zaino_grpc_listen_address),
                Some(zaino_json_listen_address),
                zaino_json_server_cookie_dir,
                Some(handle),
            )
        } else {
            (None, None, None, None)
        };

        // Launch Zingolib Lightclients:
        let clients = if enable_clients {
            let lightclient_dir = tempfile::tempdir().unwrap();
            let (lightclient_faucet, lightclient_recipient) = build_lightclients(
                lightclient_dir,
                zaino_grpc_listen_address
                    .expect("Error launching zingo lightclients. `enable_zaino` is None.")
                    .port(),
            )
            .await;
            Some(Clients {
                faucet: lightclient_faucet,
                recipient: lightclient_recipient,
            })
        } else {
            None
        };

        Ok(Self {
            local_net,
            data_dir,
            network: network_kind.into(),
            zebrad_rpc_listen_address,
            zebrad_grpc_listen_address,
            zaino_handle,
            zaino_json_rpc_listen_address: zaino_json_listen_address,
            zaino_grpc_listen_address,
            json_server_cookie_dir: zaino_json_server_cookie_dir,
            clients,
        })
    }

    /// Generates `blocks` regtest blocks.
    /// Adds a delay between blocks to allow zaino / zebra to catch up with test.
    pub async fn generate_blocks_with_delay(&self, blocks: u32) {
        for _ in 0..blocks {
            self.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
    }

    /// Closes the TestManager.
    pub async fn close(&mut self) {
        if let Some(handle) = self.zaino_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for TestManager {
    fn drop(&mut self) {
        if let Some(handle) = &self.zaino_handle {
            handle.abort();
        };
    }
}

#[cfg(test)]
mod launch_testmanager {

    use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
    use zingo_netutils::{GetClientError, GrpcConnector, UnderlyingService};

    use super::*;

    /// Builds a client for creating RPC requests to the indexer/light-node
    async fn build_client(
        uri: http::Uri,
    ) -> Result<CompactTxStreamerClient<UnderlyingService>, GetClientError> {
        GrpcConnector::new(uri).get_client().await
    }

    mod zcashd {

        use super::*;

        #[tokio::test]
        pub(crate) async fn basic() {
            let mut test_manager = TestManager::launch(
                &ValidatorKind::Zcashd,
                &BackendType::Fetch,
                None,
                None,
                false,
                false,
                false,
                true,
                true,
                false,
            )
            .await
            .unwrap();
            assert_eq!(
                1,
                u32::from(test_manager.local_net.get_chain_height().await)
            );
            test_manager.close().await;
        }

        #[tokio::test]
        pub(crate) async fn generate_blocks() {
            let mut test_manager = TestManager::launch(
                &ValidatorKind::Zcashd,
                &BackendType::Fetch,
                None,
                None,
                false,
                false,
                false,
                true,
                true,
                false,
            )
            .await
            .unwrap();
            assert_eq!(
                1,
                u32::from(test_manager.local_net.get_chain_height().await)
            );
            test_manager.local_net.generate_blocks(1).await.unwrap();
            assert_eq!(
                2,
                u32::from(test_manager.local_net.get_chain_height().await)
            );
            test_manager.close().await;
        }

        #[ignore = "chain cache needs development"]
        #[tokio::test]
        pub(crate) async fn with_chain() {
            let mut test_manager = TestManager::launch(
                &ValidatorKind::Zcashd,
                &BackendType::Fetch,
                None,
                ZCASHD_CHAIN_CACHE_DIR.clone(),
                false,
                false,
                false,
                true,
                true,
                false,
            )
            .await
            .unwrap();
            assert_eq!(
                10,
                u32::from(test_manager.local_net.get_chain_height().await)
            );
            test_manager.close().await;
        }

        #[tokio::test]
        pub(crate) async fn zaino() {
            let mut test_manager = TestManager::launch(
                &ValidatorKind::Zcashd,
                &BackendType::Fetch,
                None,
                None,
                true,
                false,
                false,
                true,
                true,
                false,
            )
            .await
            .unwrap();
            let mut grpc_client = build_client(services::network::localhost_uri(
                test_manager
                    .zaino_grpc_listen_address
                    .expect("Zaino listen port is not available but zaino is active.")
                    .port(),
            ))
            .await
            .unwrap();
            dbg!(grpc_client
                .get_lightd_info(tonic::Request::new(
                    zcash_client_backend::proto::service::Empty {},
                ))
                .await
                .unwrap());
            test_manager.close().await;
        }

        #[tokio::test]
        pub(crate) async fn zaino_clients() {
            let mut test_manager = TestManager::launch(
                &ValidatorKind::Zcashd,
                &BackendType::Fetch,
                None,
                None,
                true,
                false,
                false,
                true,
                true,
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
        #[tokio::test]
        pub(crate) async fn zaino_clients_receive_mining_reward() {
            let mut test_manager = TestManager::launch(
                &ValidatorKind::Zcashd,
                &BackendType::Fetch,
                None,
                None,
                true,
                false,
                false,
                true,
                true,
                true,
            )
            .await
            .unwrap();
            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");

            clients.faucet.sync_and_await().await.unwrap();
            dbg!(clients.faucet.do_balance().await);

            assert!(
                    clients.faucet.do_balance().await.orchard_balance.unwrap() > 0
                        || clients.faucet.do_balance().await.confirmed_transparent_balance.unwrap() > 0,
                    "No mining reward received from Zcashd. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                    clients.faucet.do_balance().await.orchard_balance.unwrap(),
                    clients.faucet.do_balance().await.confirmed_transparent_balance.unwrap()
                );

            test_manager.close().await;
        }
    }

    mod zebrad {

        use super::*;

        mod fetch_service {

            use super::*;

            #[tokio::test]
            pub(crate) async fn basic() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::Fetch,
                    None,
                    None,
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(
                    1,
                    u32::from(test_manager.local_net.get_chain_height().await)
                );
                test_manager.close().await;
            }

            #[tokio::test]
            pub(crate) async fn generate_blocks() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::Fetch,
                    None,
                    None,
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(
                    1,
                    u32::from(test_manager.local_net.get_chain_height().await)
                );
                test_manager.local_net.generate_blocks(1).await.unwrap();
                assert_eq!(
                    2,
                    u32::from(test_manager.local_net.get_chain_height().await)
                );
                test_manager.close().await;
            }

            #[ignore = "chain cache needs development"]
            #[tokio::test]
            pub(crate) async fn with_chain() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::Fetch,
                    None,
                    ZEBRAD_CHAIN_CACHE_DIR.clone(),
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(
                    52,
                    u32::from(test_manager.local_net.get_chain_height().await)
                );
                test_manager.close().await;
            }

            #[tokio::test]
            pub(crate) async fn zaino() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::Fetch,
                    None,
                    None,
                    true,
                    false,
                    false,
                    true,
                    true,
                    false,
                )
                .await
                .unwrap();
                let mut grpc_client = build_client(services::network::localhost_uri(
                    test_manager
                        .zaino_grpc_listen_address
                        .expect("Zaino listen port not available but zaino is active.")
                        .port(),
                ))
                .await
                .unwrap();
                dbg!(grpc_client
                    .get_lightd_info(tonic::Request::new(
                        zcash_client_backend::proto::service::Empty {},
                    ))
                    .await
                    .unwrap());
                test_manager.close().await;
            }

            #[tokio::test]
            pub(crate) async fn zaino_clients() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::Fetch,
                    None,
                    None,
                    true,
                    false,
                    false,
                    true,
                    true,
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
            #[tokio::test]
            pub(crate) async fn zaino_clients_receive_mining_reward() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::Fetch,
                    None,
                    None,
                    true,
                    false,
                    false,
                    true,
                    true,
                    true,
                )
                .await
                .unwrap();
                let mut clients = test_manager
                    .clients
                    .take()
                    .expect("Clients are not initialized");

                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients.faucet.do_balance().await);

                test_manager.local_net.generate_blocks(100).await.unwrap();
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients.faucet.do_balance().await);

                assert!(
                    clients.faucet.do_balance().await.orchard_balance.unwrap() > 0
                        || clients.faucet.do_balance().await.confirmed_transparent_balance.unwrap() > 0,
                    "No mining reward received from Zebrad. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                    clients.faucet.do_balance().await.orchard_balance.unwrap(),
                    clients.faucet.do_balance().await.confirmed_transparent_balance.unwrap()
            );

                test_manager.close().await;
            }

            #[tokio::test]
            pub(crate) async fn zaino_clients_receive_mining_reward_and_send() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::Fetch,
                    None,
                    None,
                    true,
                    false,
                    false,
                    true,
                    true,
                    true,
                )
                .await
                .unwrap();
                let mut clients = test_manager
                    .clients
                    .take()
                    .expect("Clients are not initialized");

                test_manager.local_net.generate_blocks(100).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients.faucet.do_balance().await);

                assert!(
                    clients
                        .faucet
                        .do_balance()
                        .await
                        .confirmed_transparent_balance
                        .unwrap()
                        > 0,
                    "No mining reward received from Zebrad. Faucet Transparent Balance: {:}.",
                    clients
                        .faucet
                        .do_balance()
                        .await
                        .confirmed_transparent_balance
                        .unwrap()
                );

                // *Send all transparent funds to own orchard address.
                clients.faucet.quick_shield().await.unwrap();
                test_manager.local_net.generate_blocks(1).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients.faucet.do_balance().await);

                assert!(
                clients.faucet.do_balance().await.orchard_balance.unwrap() > 0,
                "No funds received from shield. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                clients.faucet.do_balance().await.orchard_balance.unwrap(),
                clients.faucet.do_balance().await.confirmed_transparent_balance.unwrap()
            );

                let recipient_zaddr = clients.get_recipient_address("sapling").await;
                zingolib::testutils::lightclient::from_inputs::quick_send(
                    &mut clients.faucet,
                    vec![(&recipient_zaddr, 250_000, None)],
                )
                .await
                .unwrap();

                test_manager.local_net.generate_blocks(1).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                clients.recipient.sync_and_await().await.unwrap();
                dbg!(clients.recipient.do_balance().await);

                assert_eq!(
                    clients
                        .recipient
                        .do_balance()
                        .await
                        .verified_sapling_balance
                        .unwrap(),
                    250_000
                );

                test_manager.close().await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test]
            pub(crate) async fn zaino_testnet() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::Fetch,
                    Some(services::network::Network::Testnet),
                    ZEBRAD_TESTNET_CACHE_DIR.clone(),
                    true,
                    false,
                    false,
                    true,
                    true,
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

            #[tokio::test]
            pub(crate) async fn basic() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::State,
                    None,
                    None,
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(
                    1,
                    u32::from(test_manager.local_net.get_chain_height().await)
                );
                test_manager.close().await;
            }

            #[tokio::test]
            pub(crate) async fn generate_blocks() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::State,
                    None,
                    None,
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(
                    1,
                    u32::from(test_manager.local_net.get_chain_height().await)
                );
                test_manager.generate_blocks_with_delay(1).await;
                assert_eq!(
                    2,
                    u32::from(test_manager.local_net.get_chain_height().await)
                );
                test_manager.close().await;
            }

            #[ignore = "chain cache needs development"]
            #[tokio::test]
            pub(crate) async fn with_chain() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::State,
                    None,
                    ZEBRAD_CHAIN_CACHE_DIR.clone(),
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(
                    52,
                    u32::from(test_manager.local_net.get_chain_height().await)
                );
                test_manager.close().await;
            }

            #[tokio::test]
            pub(crate) async fn zaino() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::State,
                    None,
                    None,
                    true,
                    false,
                    false,
                    true,
                    true,
                    false,
                )
                .await
                .unwrap();
                let mut grpc_client = build_client(services::network::localhost_uri(
                    test_manager
                        .zaino_grpc_listen_address
                        .expect("Zaino listen port not available but zaino is active.")
                        .port(),
                ))
                .await
                .unwrap();
                dbg!(grpc_client
                    .get_lightd_info(tonic::Request::new(
                        zcash_client_backend::proto::service::Empty {},
                    ))
                    .await
                    .unwrap());
                test_manager.close().await;
            }

            #[tokio::test]
            pub(crate) async fn zaino_clients() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::State,
                    None,
                    None,
                    true,
                    false,
                    false,
                    true,
                    true,
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
            #[ignore = "Bug in zingolib 1.0 sync, reinstate on zinglib 2.0 upgrade."]
            #[tokio::test]
            pub(crate) async fn zaino_clients_receive_mining_reward() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::State,
                    None,
                    None,
                    true,
                    false,
                    false,
                    true,
                    true,
                    true,
                )
                .await
                .unwrap();

                let mut clients = test_manager
                    .clients
                    .take()
                    .expect("Clients are not initialized");

                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients.faucet.do_balance().await);

                test_manager.generate_blocks_with_delay(100).await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients.faucet.do_balance().await);

                assert!(
                    clients.faucet.do_balance().await.orchard_balance.unwrap() > 0
                        || clients.faucet.do_balance().await.confirmed_transparent_balance.unwrap() > 0,
                    "No mining reward received from Zebrad. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                    clients.faucet.do_balance().await.orchard_balance.unwrap(),
                    clients.faucet.do_balance().await.confirmed_transparent_balance.unwrap()
            );

                test_manager.close().await;
            }

            #[ignore = "Bug in zingolib 1.0 sync, reinstate on zinglib 2.0 upgrade."]
            #[tokio::test]
            pub(crate) async fn zaino_clients_receive_mining_reward_and_send() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::State,
                    None,
                    None,
                    true,
                    false,
                    false,
                    true,
                    true,
                    true,
                )
                .await
                .unwrap();

                let mut clients = test_manager
                    .clients
                    .take()
                    .expect("Clients are not initialized");

                test_manager.generate_blocks_with_delay(100).await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients.faucet.do_balance().await);

                assert!(
                    clients
                        .faucet
                        .do_balance()
                        .await
                        .confirmed_transparent_balance
                        .unwrap()
                        > 0,
                    "No mining reward received from Zebrad. Faucet Transparent Balance: {:}.",
                    clients
                        .faucet
                        .do_balance()
                        .await
                        .confirmed_transparent_balance
                        .unwrap()
                );

                // *Send all transparent funds to own orchard address.
                clients.faucet.quick_shield().await.unwrap();
                test_manager.generate_blocks_with_delay(1).await;
                clients.faucet.sync_and_await().await.unwrap();
                dbg!(clients.faucet.do_balance().await);

                assert!(
                clients.faucet.do_balance().await.orchard_balance.unwrap() > 0,
                "No funds received from shield. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                clients.faucet.do_balance().await.orchard_balance.unwrap(),
                clients.faucet.do_balance().await.confirmed_transparent_balance.unwrap()
            );

                let recipient_zaddr = clients.get_recipient_address("sapling").await;
                zingolib::testutils::lightclient::from_inputs::quick_send(
                    &mut clients.faucet,
                    vec![(&recipient_zaddr, 250_000, None)],
                )
                .await
                .unwrap();

                test_manager.generate_blocks_with_delay(1).await;
                clients.recipient.sync_and_await().await.unwrap();
                dbg!(clients.recipient.do_balance().await);

                assert_eq!(
                    clients
                        .recipient
                        .do_balance()
                        .await
                        .verified_sapling_balance
                        .unwrap(),
                    250_000
                );

                test_manager.close().await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test]
            pub(crate) async fn zaino_testnet() {
                let mut test_manager = TestManager::launch(
                    &ValidatorKind::Zebrad,
                    &BackendType::State,
                    Some(services::network::Network::Testnet),
                    ZEBRAD_TESTNET_CACHE_DIR.clone(),
                    true,
                    false,
                    false,
                    true,
                    true,
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
