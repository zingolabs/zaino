//! Zaino Testing Utilities.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use once_cell::sync::Lazy;
use services::validator::Validator;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    str::FromStr,
};
use tempfile::TempDir;
use testvectors::REG_O_ADDR_FROM_ABANDONART;
use tracing_subscriber::EnvFilter;

/// Path for zcashd binary.
pub static ZCASHD_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| {
    let mut workspace_root_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    workspace_root_path.pop();
    Some(workspace_root_path.join("test_binaries/bins/zcashd"))
});

/// Path for zcash-cli binary.
pub static ZCASH_CLI_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| {
    let mut workspace_root_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    workspace_root_path.pop();
    Some(workspace_root_path.join("test_binaries/bins/zcash-cli"))
});

/// Path for zebrad binary.
pub static ZEBRAD_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| {
    let mut workspace_root_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    workspace_root_path.pop();
    Some(workspace_root_path.join("test_binaries/bins/zebrad"))
});

/// Path for lightwalletd binary.
pub static LIGHTWALLETD_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| {
    let mut workspace_root_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    workspace_root_path.pop();
    Some(workspace_root_path.join("test_binaries/bins/lightwalletd"))
});

/// Path for zainod binary.
pub static ZAINOD_BIN: Lazy<Option<PathBuf>> = Lazy::new(|| {
    let mut workspace_root_path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    workspace_root_path.pop();
    Some(workspace_root_path.join("target/release/zainod"))
});

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

/// Represents the type of validator to launch.
pub enum ValidatorKind {
    /// Zcashd.
    Zcashd,
    /// Zebrad.
    Zebrad,
}

impl std::str::FromStr for ValidatorKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "zcashd" => Ok(ValidatorKind::Zcashd),
            "zebrad" => Ok(ValidatorKind::Zebrad),
            _ => Err(format!("Invalid validator kind: {}", s)),
        }
    }
}

/// Config for validators.
pub enum ValidatorConfig {
    /// Zcashd Config.
    ZcashdConfig(services::validator::ZcashdConfig),
    /// Zebrad Config.
    ZebradConfig(services::validator::ZebradConfig),
}

/// Available zcash-local-net configurations.
#[allow(
    clippy::large_enum_variant,
    reason = "Maybe this issue: https://github.com/rust-lang/rust-clippy/issues/9798"
)]
pub enum LocalNet {
    /// Zcash-local-net backed by Zcashd.
    Zcashd(services::LocalNet<services::indexer::Empty, services::validator::Zcashd>),
    /// Zcash-local-net backed by Zebrad.
    Zebrad(services::LocalNet<services::indexer::Empty, services::validator::Zebrad>),
}

impl services::validator::Validator for LocalNet {
    const CONFIG_FILENAME: &str = "";

    type Config = ValidatorConfig;

    #[allow(clippy::manual_async_fn)]
    fn launch(
        config: Self::Config,
    ) -> impl std::future::Future<Output = Result<Self, services::error::LaunchError>> + Send {
        async move {
            match config {
                ValidatorConfig::ZcashdConfig(cfg) => {
                    let net = services::LocalNet::<
                        services::indexer::Empty,
                        services::validator::Zcashd,
                    >::launch(services::indexer::EmptyConfig {}, cfg)
                    .await;
                    Ok(LocalNet::Zcashd(net))
                }
                ValidatorConfig::ZebradConfig(cfg) => {
                    let net = services::LocalNet::<
                        services::indexer::Empty,
                        services::validator::Zebrad,
                    >::launch(services::indexer::EmptyConfig {}, cfg)
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

    fn network(&self) -> services::network::Network {
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
        validator_network: services::network::Network,
    ) -> PathBuf {
        if chain_cache.to_string_lossy().contains("zcashd") {
            services::validator::Zcashd::load_chain(
                chain_cache,
                validator_data_dir,
                validator_network,
            )
        } else if chain_cache.to_string_lossy().contains("zebrad") {
            services::validator::Zebrad::load_chain(
                chain_cache,
                validator_data_dir,
                validator_network,
            )
        } else {
            panic!(
                "Invalid chain_cache path: expected to contain 'zcashd' or 'zebrad', but got: {}",
                chain_cache.display()
            );
        }
    }
}

/// Holds zingo lightclients along with their TempDir for wallet-2-validator tests.
pub struct Clients {
    /// Lightclient TempDir location.
    pub lightclient_dir: TempDir,
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
    pub network: services::network::Network,
    /// Zebrad/Zcashd JsonRpc listen address.
    pub zebrad_rpc_listen_address: SocketAddr,
    /// Zaino Indexer JoinHandle.
    pub zaino_handle: Option<tokio::task::JoinHandle<Result<(), zainodlib::error::IndexerError>>>,
    /// Zingo-Indexer gRPC listen address.
    pub zaino_grpc_listen_address: Option<SocketAddr>,
    /// Zingolib lightclients.
    pub clients: Option<Clients>,
}

use zingo_infra_testutils::services;
impl TestManager {
    /// Launches zcash-local-net<Empty, Validator>.
    ///
    /// Possible validators: Zcashd, Zebrad.
    ///
    /// If chain_cache is given a path the chain will be loaded.
    ///
    /// If clients is set to active zingolib lightclients will be created for test use.
    pub async fn launch(
        validator: &str,
        network: Option<services::network::Network>,
        chain_cache: Option<PathBuf>,
        enable_zaino: bool,
        zaino_no_sync: bool,
        zaino_no_db: bool,
        enable_clients: bool,
    ) -> Result<Self, std::io::Error> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
            .with_target(true)
            .try_init();

        let validator_kind = ValidatorKind::from_str(validator).unwrap();
        let network = network.unwrap_or(services::network::Network::Regtest);
        if enable_clients && !enable_zaino {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Cannot enable clients when zaino is not enabled.",
            ));
        }

        // Launch LocalNet:
        let zebrad_rpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
        let zebrad_rpc_listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zebrad_rpc_listen_port);

        let validator_config = match validator_kind {
            ValidatorKind::Zcashd => {
                let cfg = services::validator::ZcashdConfig {
                    zcashd_bin: ZCASHD_BIN.clone(),
                    zcash_cli_bin: ZCASH_CLI_BIN.clone(),
                    rpc_listen_port: Some(zebrad_rpc_listen_port),
                    activation_heights: services::network::ActivationHeights::default(),
                    miner_address: Some(REG_O_ADDR_FROM_ABANDONART),
                    chain_cache,
                };
                ValidatorConfig::ZcashdConfig(cfg)
            }
            ValidatorKind::Zebrad => {
                let cfg = services::validator::ZebradConfig {
                    zebrad_bin: ZEBRAD_BIN.clone(),
                    network_listen_port: None,
                    rpc_listen_port: Some(zebrad_rpc_listen_port),
                    activation_heights: services::network::ActivationHeights::default(),
                    miner_address: services::validator::ZEBRAD_DEFAULT_MINER,
                    chain_cache,
                    network,
                };
                ValidatorConfig::ZebradConfig(cfg)
            }
        };
        let local_net = LocalNet::launch(validator_config).await.unwrap();
        let data_dir = local_net.data_dir().path().to_path_buf();
        let db_path = data_dir.join("zaino");

        // Launch Zaino:
        let (zaino_grpc_listen_address, zaino_handle) = if enable_zaino {
            let zaino_grpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
            let zaino_grpc_listen_address =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zaino_grpc_listen_port);

            let indexer_config = zainodlib::config::IndexerConfig {
                grpc_listen_address: zaino_grpc_listen_address,
                grpc_tls: false,
                tls_cert_path: None,
                tls_key_path: None,
                validator_listen_address: zebrad_rpc_listen_address,
                validator_cookie_auth: false,
                validator_cookie_path: None,
                validator_user: Some("xxxxxx".to_string()),
                validator_password: Some("xxxxxx".to_string()),
                map_capacity: None,
                map_shard_amount: None,
                db_path,
                db_size: None,
                network: network.to_string(),
                no_sync: zaino_no_sync,
                no_db: zaino_no_db,
                no_state: false,
            };
            let handle = zainodlib::indexer::Indexer::spawn(indexer_config)
                .await
                .unwrap();

            // NOTE: This is required to give the server time to launch, this is not used in production code but could be rewritten to improve testing efficiency.
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            (Some(zaino_grpc_listen_address), Some(handle))
        } else {
            (None, None)
        };

        // Launch Zingolib Lightclients:
        let clients = if enable_clients {
            let lightclient_dir = tempfile::tempdir().unwrap();
            let lightclients = zingo_infra_testutils::client::build_lightclients(
                lightclient_dir.path().to_path_buf(),
                zaino_grpc_listen_address
                    .expect("Error launching zingo lightclients. `enable_zaino` is None.")
                    .port(),
            )
            .await;
            Some(Clients {
                lightclient_dir,
                faucet: lightclients.0,
                recipient: lightclients.1,
            })
        } else {
            None
        };

        Ok(Self {
            local_net,
            data_dir,
            network,
            zebrad_rpc_listen_address,
            zaino_handle,
            zaino_grpc_listen_address,
            clients,
        })
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn launch_testmanager_zebrad() {
        let mut test_manager = TestManager::launch("zebrad", None, None, false, true, true, false)
            .await
            .unwrap();
        assert_eq!(
            1,
            u32::from(test_manager.local_net.get_chain_height().await)
        );
        test_manager.close().await;
    }

    #[tokio::test]
    async fn launch_testmanager_zcashd() {
        let mut test_manager = TestManager::launch("zcashd", None, None, false, true, true, false)
            .await
            .unwrap();
        assert_eq!(
            1,
            u32::from(test_manager.local_net.get_chain_height().await)
        );
        test_manager.close().await;
    }

    #[tokio::test]
    async fn launch_testmanager_zebrad_generate_blocks() {
        let mut test_manager = TestManager::launch("zebrad", None, None, false, true, true, false)
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

    #[tokio::test]
    async fn launch_testmanager_zcashd_generate_blocks() {
        let mut test_manager = TestManager::launch("zcashd", None, None, false, true, true, false)
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

    #[tokio::test]
    async fn launch_testmanager_zebrad_with_chain() {
        let mut test_manager = TestManager::launch(
            "zebrad",
            None,
            ZEBRAD_CHAIN_CACHE_DIR.clone(),
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
    async fn launch_testmanager_zcashd_with_chain() {
        let mut test_manager = TestManager::launch(
            "zcashd",
            None,
            ZCASHD_CHAIN_CACHE_DIR.clone(),
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
    async fn launch_testmanager_zebrad_zaino() {
        let mut test_manager = TestManager::launch("zebrad", None, None, true, true, true, false)
            .await
            .unwrap();
        let mut grpc_client =
            zingo_infra_testutils::client::build_client(services::network::localhost_uri(
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
    async fn launch_testmanager_zcashd_zaino() {
        let mut test_manager = TestManager::launch("zcashd", None, None, true, true, true, false)
            .await
            .unwrap();
        let mut grpc_client =
            zingo_infra_testutils::client::build_client(services::network::localhost_uri(
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
    async fn launch_testmanager_zebrad_zaino_clients() {
        let mut test_manager = TestManager::launch("zebrad", None, None, true, true, true, true)
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

    #[tokio::test]
    async fn launch_testmanager_zcashd_zaino_clients() {
        let mut test_manager = TestManager::launch("zcashd", None, None, true, true, true, true)
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
    async fn launch_testmanager_zebrad_zaino_clients_receive_mining_reward() {
        let mut test_manager = TestManager::launch("zebrad", None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();
        dbg!(clients.faucet.do_balance().await);

        test_manager.local_net.generate_blocks(100).await.unwrap();
        clients.faucet.do_sync(true).await.unwrap();
        dbg!(clients.faucet.do_balance().await);

        assert!(
                clients.faucet.do_balance().await.orchard_balance.unwrap() > 0
                    || clients.faucet.do_balance().await.transparent_balance.unwrap() > 0,
                "No mining reward received from Zebrad. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                clients.faucet.do_balance().await.orchard_balance.unwrap(),
                clients.faucet.do_balance().await.transparent_balance.unwrap()
        );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn launch_testmanager_zcashd_zaino_clients_receive_mining_reward() {
        let mut test_manager = TestManager::launch("zcashd", None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();
        dbg!(clients.faucet.do_balance().await);

        assert!(
                clients.faucet.do_balance().await.orchard_balance.unwrap() > 0
                    || clients.faucet.do_balance().await.transparent_balance.unwrap() > 0,
                "No mining reward received from Zcashd. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
                clients.faucet.do_balance().await.orchard_balance.unwrap(),
                clients.faucet.do_balance().await.transparent_balance.unwrap()
            );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn launch_testmanager_zebrad_zaino_clients_receive_mining_reward_and_send() {
        let mut test_manager = TestManager::launch("zebrad", None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        dbg!(clients.faucet.do_balance().await);

        assert!(
            clients
                .faucet
                .do_balance()
                .await
                .transparent_balance
                .unwrap()
                > 0,
            "No mining reward received from Zebrad. Faucet Transparent Balance: {:}.",
            clients
                .faucet
                .do_balance()
                .await
                .transparent_balance
                .unwrap()
        );

        // *Send all transparent funds to own orchard address.
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        dbg!(clients.faucet.do_balance().await);

        assert!(
            clients.faucet.do_balance().await.orchard_balance.unwrap() > 0,
            "No funds received from shield. Faucet Orchard Balance: {:}. Faucet Transparent Balance: {:}.",
            clients.faucet.do_balance().await.orchard_balance.unwrap(),
            clients.faucet.do_balance().await.transparent_balance.unwrap()
        );

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("sapling").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.recipient.do_sync(true).await.unwrap();
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

    #[tokio::test]
    async fn launch_testmanager_zcashd_zaino_clients_receive_mining_reward_and_send() {
        let mut test_manager = TestManager::launch("zcashd", None, None, true, true, true, true)
            .await
            .unwrap();
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();
        dbg!(clients.faucet.do_balance().await);

        assert!(
            clients
                .faucet
                .do_balance()
                .await
                .verified_orchard_balance
                .unwrap()
                > 0,
            "No mining reward received from Zcashd. Faucet Orchard Balance: {:}.",
            clients
                .faucet
                .do_balance()
                .await
                .verified_orchard_balance
                .unwrap()
        );

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("sapling").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.recipient.do_sync(true).await.unwrap();
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

    #[tokio::test]
    async fn launch_testmanager_zebrad_zaino_testnet() {
        let mut test_manager = TestManager::launch(
            "zebrad",
            Some(services::network::Network::Testnet),
            ZEBRAD_TESTNET_CACHE_DIR.clone(),
            true,
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
