//! Zaino Testing Utilities.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use once_cell::sync::Lazy;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};
use tempfile::TempDir;
use testvectors::{seeds, REG_O_ADDR_FROM_ABANDONART};
use tracing_subscriber::EnvFilter;
use zaino_commons::config::{
    AuthMethod, BackendType, BlockCacheConfig, CacheConfig, DatabaseConfig, ServiceConfig,
    ValidatorConfig as ZainoValidatorConfig, ZainoStateConfig,
};
use zainodlib::config::default_ephemeral_cookie_path;
pub use zingo_infra_services as services;
use zingo_infra_services::network::Network;
pub use zingo_infra_services::validator::Validator;
use zingolib::{config::RegtestNetwork, testutils::scenarios::setup::ClientBuilder};
pub use zingolib::{
    get_base_address_macro, lightclient::LightClient, testutils::lightclient::from_inputs,
};

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

#[derive(Debug, PartialEq, Clone, Copy)]
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
    ZcashdConfig(zingo_infra_services::validator::ZcashdConfig),
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

    type Config = ValidatorConfig;

    fn activation_heights(&self) -> zingo_infra_services::network::ActivationHeights {
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

    fn network(&self) -> Network {
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
        validator_network: Network,
    ) -> PathBuf {
        if chain_cache.to_string_lossy().contains("zcashd") {
            zingo_infra_services::validator::Zcashd::load_chain(
                chain_cache,
                validator_data_dir,
                validator_network,
            )
        } else if chain_cache.to_string_lossy().contains("zebrad") {
            zingo_infra_services::validator::Zebrad::load_chain(
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

/// Authentication configuration for tests.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Authentication method for validator connection
    pub validator_auth: Option<AuthMethod>,
    /// Authentication method for JSON-RPC server
    pub json_server_auth: Option<AuthMethod>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            validator_auth: None,   // Will use default basic auth
            json_server_auth: None, // Will use no auth
        }
    }
}

/// Configuration for Zaino services in tests.
#[derive(Debug, Clone)]
pub struct ZainoConfig {
    /// Enable Zaino indexer
    pub enable_zaino: bool,
    /// Enable JsonRPC server
    pub enable_json_server: bool,
    /// Enable cookie authentication for JsonRPC server
    pub enable_json_server_cookie_auth: bool,
    /// Disable sync (for testing)
    pub no_sync: bool,
    /// Disable database (for testing)
    pub no_db: bool,
}

impl Default for ZainoConfig {
    fn default() -> Self {
        Self {
            enable_zaino: true,
            enable_json_server: false,
            enable_json_server_cookie_auth: false,
            no_sync: true,
            no_db: true,
        }
    }
}

/// Configuration for lightclients in tests.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Enable zingolib lightclients
    pub enable_clients: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            enable_clients: false,
        }
    }
}

/// Comprehensive configuration for TestManager.
#[derive(Clone)]
pub struct TestManagerConfig {
    /// Type of validator to launch
    pub validator_kind: ValidatorKind,
    /// Backend type for Zaino
    pub backend_type: BackendType,
    /// Network configuration
    pub network: Option<zaino_commons::config::Network>,
    /// Optional chain cache directory
    pub chain_cache: Option<PathBuf>,
    /// Zaino configuration
    pub zaino_config: ZainoConfig,
    /// Client configuration  
    pub client_config: ClientConfig,
    /// Authentication configuration
    pub auth_config: AuthConfig,
}

impl TestManagerConfig {
    /// Create minimal test configuration (validator only)
    pub fn minimal(validator_kind: ValidatorKind, backend_type: BackendType) -> Self {
        Self {
            validator_kind,
            backend_type,
            network: None,
            chain_cache: None,
            zaino_config: ZainoConfig {
                enable_zaino: false,
                ..Default::default()
            },
            client_config: ClientConfig::default(),
            auth_config: AuthConfig::default(),
        }
    }

    /// Create configuration with Zaino indexer enabled
    pub fn with_zaino(validator_kind: ValidatorKind, backend_type: BackendType) -> Self {
        Self {
            validator_kind,
            backend_type,
            network: None,
            chain_cache: None,
            zaino_config: ZainoConfig::default(),
            client_config: ClientConfig::default(),
            auth_config: AuthConfig::default(),
        }
    }

    /// Create full stack configuration (Zaino + clients)
    pub fn full_stack(validator_kind: ValidatorKind, backend_type: BackendType) -> Self {
        Self {
            validator_kind,
            backend_type,
            network: None,
            chain_cache: None,
            zaino_config: ZainoConfig::default(),
            client_config: ClientConfig {
                enable_clients: true,
            },
            auth_config: AuthConfig::default(),
        }
    }

    /// Create configuration with chain cache
    pub fn with_chain_cache(
        validator_kind: ValidatorKind,
        backend_type: BackendType,
        chain_cache: PathBuf,
    ) -> Self {
        Self {
            validator_kind,
            backend_type,
            network: None,
            chain_cache: Some(chain_cache),
            zaino_config: ZainoConfig::default(),
            client_config: ClientConfig::default(),
            auth_config: AuthConfig::default(),
        }
    }

    /// Set network type
    pub fn with_network(mut self, network: zaino_commons::config::Network) -> Self {
        self.network = Some(network);
        self
    }

    /// Enable JSON-RPC server
    pub fn with_json_server(mut self, enable_cookie_auth: bool) -> Self {
        self.zaino_config.enable_json_server = true;
        self.zaino_config.enable_json_server_cookie_auth = enable_cookie_auth;
        self
    }

    /// Enable sync and database (disable testing flags)
    pub fn with_sync_and_db(mut self) -> Self {
        self.zaino_config.no_sync = false;
        self.zaino_config.no_db = false;
        self
    }

    // ===== Ergonomic Constructor Methods for Common Test Patterns =====

    /// Configuration for wallet integration tests
    /// 
    /// Enables: Zaino indexer, clients, no sync/no db (testing flags)
    /// Perfect for: wallet_to_validator.rs tests, send/receive/shield operations
    pub fn for_wallet_tests(validator_kind: ValidatorKind, backend_type: BackendType) -> Self {
        Self::with_zaino(validator_kind, backend_type)
            .with_clients()
            .with_no_sync_no_db() // Wallet tests use testing flags
    }

    /// Configuration for JSON server tests
    ///
    /// Enables: Zaino indexer, JSON server with optional cookie auth, sync and database
    /// Perfect for: json_server.rs tests, RPC method testing
    pub fn for_json_server_tests(validator_kind: ValidatorKind, enable_cookie_auth: bool) -> Self {
        Self::with_zaino(validator_kind, BackendType::Fetch) // JSON server tests typically use Fetch
            .with_json_server(enable_cookie_auth)
            .with_sync_and_db()
    }

    /// Configuration for basic infrastructure tests
    ///
    /// Enables: Zaino indexer only, minimal configuration
    /// Perfect for: basic connectivity, service spawn tests
    pub fn for_basic_tests(validator_kind: ValidatorKind, backend_type: BackendType) -> Self {
        Self::with_zaino(validator_kind, backend_type)
            .with_sync_and_db()
    }

    /// Configuration for chain cache tests
    ///
    /// Enables: Zaino indexer, chain cache, no sync/db for testing
    /// Perfect for: chain_cache.rs tests, cached data scenarios
    pub fn for_chain_cache_tests(
        validator_kind: ValidatorKind, 
        chain_cache: PathBuf
    ) -> Self {
        Self::with_chain_cache(validator_kind, BackendType::Fetch, chain_cache)
            // Chain cache tests often test with no_sync and no_db flags
    }

    /// Configuration for state service tests  
    ///
    /// Enables: State backend, Zaino indexer, sync and database
    /// Perfect for: state_service.rs, local_cache.rs tests
    pub fn for_state_tests(validator_kind: ValidatorKind) -> Self {
        Self::with_zaino(validator_kind, BackendType::State)
            .with_sync_and_db()
    }

    /// Enable clients (chainable method)
    pub fn with_clients(mut self) -> Self {
        self.client_config.enable_clients = true;
        self
    }

    /// Conditionally enable clients (chainable method)
    pub fn with_clients_if(mut self, enable: bool) -> Self {
        self.client_config.enable_clients = enable;
        self
    }

    /// Disable sync and database (for testing scenarios)
    pub fn with_no_sync_no_db(mut self) -> Self {
        self.zaino_config.no_sync = true;
        self.zaino_config.no_db = true;
        self
    }

    /// Set validator authentication method
    pub fn with_validator_auth(mut self, auth: AuthMethod) -> Self {
        self.auth_config.validator_auth = Some(auth);
        self
    }

    /// Set JSON server authentication method
    pub fn with_json_server_auth(mut self, auth: AuthMethod) -> Self {
        self.auth_config.json_server_auth = Some(auth);
        self
    }

    /// Enable cookie authentication for both validator and JSON server
    pub fn with_cookie_auth(
        mut self,
        validator_cookie_path: PathBuf,
        server_cookie_path: Option<PathBuf>,
    ) -> Self {
        self.auth_config.validator_auth = Some(AuthMethod::Cookie {
            path: validator_cookie_path,
        });

        if let Some(server_path) = server_cookie_path {
            self.auth_config.json_server_auth = Some(AuthMethod::Cookie { path: server_path });
        }
        self
    }

    /// Enable basic authentication with custom credentials
    pub fn with_basic_auth(mut self, username: String, password: String) -> Self {
        self.auth_config.validator_auth = Some(AuthMethod::Basic {
            username: username.clone(),
            password: password.clone(),
        });

        self.auth_config.json_server_auth = Some(AuthMethod::Basic { username, password });
        self
    }
}

impl std::fmt::Debug for TestManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestManagerConfig")
            .field("validator_kind", &self.validator_kind)
            .field("backend_type", &self.backend_type)
            .field("network", &"Network") // Just print "Network" since it doesn't implement Debug
            .field("chain_cache", &self.chain_cache)
            .field("zaino_config", &self.zaino_config)
            .field("client_config", &self.client_config)
            .field("auth_config", &self.auth_config)
            .finish()
    }
}

/// Configuration data for Zingo-Indexer Tests.
pub struct TestManager {
    /// Zcash-local-net.
    pub local_net: LocalNet,
    /// Data directory for the validator.
    pub data_dir: PathBuf,
    /// Network (chain) type:
    pub network: zaino_commons::config::Network,
    /// Zebrad/Zcashd JsonRpc listen address.
    pub zebrad_rpc_listen_address: SocketAddr,
    /// Zebrad/Zcashd gRpc listen address.
    pub zebrad_grpc_listen_address: SocketAddr,
    /// Validator configuration for Zaino.
    pub validator_config: ZainoValidatorConfig,
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
    lightclient_dir: PathBuf,
    indexer_port: portpicker::Port,
) -> (LightClient, LightClient) {
    let mut client_builder = ClientBuilder::new(make_uri(indexer_port), lightclient_dir);
    let faucet = client_builder.build_faucet(true, RegtestNetwork::all_upgrades_active());
    let recipient = client_builder.build_client(
        seeds::HOSPITAL_MUSEUM_SEED.to_string(),
        1,
        true,
        RegtestNetwork::all_upgrades_active(),
    );

    (faucet, recipient)
}
impl TestManager {
    /// Create a ValidatorConfig for Zaino from test addresses and auth config.
    fn create_validator_config(
        zebrad_rpc_listen_address: SocketAddr,
        zebrad_grpc_listen_address: SocketAddr,
        auth_config: &AuthConfig,
    ) -> ZainoValidatorConfig {
        let auth = auth_config
            .validator_auth
            .clone()
            .unwrap_or_else(|| AuthMethod::default());

        ZainoValidatorConfig {
            config: ZainoStateConfig::default(),
            rpc_address: zebrad_rpc_listen_address,
            indexer_rpc_address: zebrad_grpc_listen_address,
            auth,
        }
    }

    /// Create IndexerConfig from TestManagerConfig and addresses.
    fn create_indexer_config(
        config: &TestManagerConfig,
        validator_config: &ZainoValidatorConfig,
        zaino_grpc_listen_address: SocketAddr,
        zaino_json_listen_address: SocketAddr,
        zaino_db_path: PathBuf,
        zebra_db_path: PathBuf,
        zaino_json_server_cookie_dir: Option<PathBuf>,
    ) -> zainodlib::config::IndexerConfig {
        let network = config
            .network
            .unwrap_or(zaino_commons::config::Network::Regtest);

        zainodlib::config::IndexerConfig {
            backend: config.backend_type,
            network,
            server: zainodlib::config::ServerConfig {
                json_rpc: if config.zaino_config.enable_json_server {
                    Some(zaino_commons::config::JsonRpcConfig {
                        listen_address: zaino_json_listen_address,
                        auth: if config.zaino_config.enable_json_server_cookie_auth {
                            zaino_commons::config::CookieAuth::Enabled {
                                path: zaino_json_server_cookie_dir
                                    .unwrap_or_else(|| PathBuf::from("/tmp/zaino.cookie")),
                            }
                        } else {
                            zaino_commons::config::CookieAuth::Disabled
                        },
                    })
                } else {
                    None
                },
                grpc: zaino_commons::config::GrpcConfig {
                    listen_address: zaino_grpc_listen_address,
                    tls: zaino_commons::config::TlsConfig::Disabled,
                },
            },
            validator: validator_config.clone(),
            service: ServiceConfig::default(),
            storage: zainodlib::config::StorageConfig {
                cache: CacheConfig {
                    capacity: None,
                    shard_amount: None,
                },
                zaino_database: DatabaseConfig {
                    path: zaino_db_path,
                    size: None,
                },
                zebra_database: DatabaseConfig {
                    path: zebra_db_path,
                    size: None,
                },
            },
            debug: zainodlib::config::DebugConfig {
                no_sync: config.zaino_config.no_sync,
                no_db: config.zaino_config.no_db,
                slow_sync: false,
            },
        }
    }

    /// Launch with new TestManagerConfig structure.
    pub async fn launch_with_config(config: TestManagerConfig) -> Result<Self, std::io::Error> {
        // Validation
        if (config.validator_kind == ValidatorKind::Zcashd)
            && (config.backend_type == BackendType::State)
        {
            return Err(std::io::Error::other(
                "Cannot use state backend with zcashd.",
            ));
        }

        if config.client_config.enable_clients && !config.zaino_config.enable_zaino {
            return Err(std::io::Error::other(
                "Cannot enable clients when zaino is not enabled.",
            ));
        }

        // Initialize logging
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
            .with_target(true)
            .try_init();

        let network = config
            .network
            .unwrap_or(zaino_commons::config::Network::Regtest);

        // Set up network ports
        let zebrad_rpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
        let zebrad_grpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
        let zebrad_rpc_listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zebrad_rpc_listen_port);
        let zebrad_grpc_listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zebrad_grpc_listen_port);

        // Create validator config for zaino
        let validator_config = Self::create_validator_config(
            zebrad_rpc_listen_address,
            zebrad_grpc_listen_address,
            &config.auth_config,
        );

        // Launch LocalNet
        let local_net_validator_config = match config.validator_kind {
            ValidatorKind::Zcashd => {
                let cfg = zingo_infra_services::validator::ZcashdConfig {
                    zcashd_bin: ZCASHD_BIN.clone(),
                    zcash_cli_bin: ZCASH_CLI_BIN.clone(),
                    rpc_listen_port: Some(zebrad_rpc_listen_port),
                    activation_heights: zingo_infra_services::network::ActivationHeights::default(),
                    miner_address: Some(REG_O_ADDR_FROM_ABANDONART),
                    chain_cache: config.chain_cache.clone(),
                };
                ValidatorConfig::ZcashdConfig(cfg)
            }
            ValidatorKind::Zebrad => {
                let cfg = zingo_infra_services::validator::ZebradConfig {
                    zebrad_bin: ZEBRAD_BIN.clone(),
                    network_listen_port: None,
                    rpc_listen_port: Some(zebrad_rpc_listen_port),
                    indexer_listen_port: Some(zebrad_grpc_listen_port),
                    activation_heights: zingo_infra_services::network::ActivationHeights::default(),
                    miner_address: zingo_infra_services::validator::ZEBRAD_DEFAULT_MINER,
                    chain_cache: config.chain_cache.clone(),
                    network: network.into(),
                };
                ValidatorConfig::ZebradConfig(cfg)
            }
        };

        let local_net = LocalNet::launch(local_net_validator_config).await.unwrap();
        let data_dir = local_net.data_dir().path().to_path_buf();
        let zaino_db_path = data_dir.join("zaino");

        let zebra_db_path = match config.chain_cache.clone() {
            Some(cache) => cache,
            None => data_dir.clone(),
        };

        // Launch Zaino if enabled
        let (
            zaino_grpc_listen_address,
            zaino_json_listen_address,
            zaino_json_server_cookie_dir,
            zaino_handle,
        ) = if config.zaino_config.enable_zaino {
            let zaino_grpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
            let zaino_grpc_listen_address =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zaino_grpc_listen_port);

            let zaino_json_listen_port = portpicker::pick_unused_port().expect("No ports free");
            let zaino_json_listen_address =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zaino_json_listen_port);
            let zaino_json_server_cookie_dir = Some(default_ephemeral_cookie_path());

            let indexer_config = Self::create_indexer_config(
                &config,
                &validator_config,
                zaino_grpc_listen_address,
                zaino_json_listen_address,
                zaino_db_path,
                zebra_db_path,
                zaino_json_server_cookie_dir.clone(),
            );

            let handle = zainodlib::indexer::spawn_indexer(indexer_config)
                .await
                .unwrap();

            // NOTE: This is required to give the server time to launch
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

        // Launch Zingolib Lightclients if enabled
        let clients = if config.client_config.enable_clients {
            let lightclient_dir = tempfile::tempdir().unwrap();
            let lightclients = build_lightclients(
                lightclient_dir.path().to_path_buf(),
                zaino_grpc_listen_address
                    .expect("Error launching zingo lightclients. zaino is not enabled.")
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
            zebrad_grpc_listen_address,
            validator_config,
            zaino_handle,
            zaino_json_rpc_listen_address: zaino_json_listen_address,
            zaino_grpc_listen_address,
            json_server_cookie_dir: zaino_json_server_cookie_dir,
            clients,
        })
    }
    /// Launches zcash-local-net<Empty, Validator>.
    ///
    /// Possible validators: Zcashd, Zebrad.
    ///
    /// If chain_cache is given a path the chain will be loaded.
    ///
    /// If clients is set to active zingolib lightclients will be created for test use.
    ///
    /// # Deprecated
    /// This method is deprecated. Use `launch_with_config()` with `TestManagerConfig` constructors instead:
    /// - `TestManagerConfig::for_wallet_tests()` - for wallet integration tests
    /// - `TestManagerConfig::for_json_server_tests()` - for JSON RPC server tests  
    /// - `TestManagerConfig::for_basic_tests()` - for basic infrastructure tests
    /// - `TestManagerConfig::for_chain_cache_tests()` - for chain cache tests
    /// - `TestManagerConfig::for_state_tests()` - for state service tests
    #[deprecated(since = "0.2.0", note = "Use `launch_with_config()` with `TestManagerConfig` constructors instead")]
    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        validator: &ValidatorKind,
        backend: &BackendType,
        network: Option<services::network::Network>,
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

        let network = network.unwrap_or(services::network::Network::Regtest);
        if enable_clients && !enable_zaino {
            return Err(std::io::Error::other(
                "Cannot enable clients when zaino is not enabled.",
            ));
        }

        // Launch LocalNet:
        let zebrad_rpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
        let zebrad_grpc_listen_port = portpicker::pick_unused_port().expect("No ports free");
        let zebrad_rpc_listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zebrad_rpc_listen_port);
        let zebrad_grpc_listen_address =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), zebrad_grpc_listen_port);

        let validator_config = match validator {
            ValidatorKind::Zcashd => {
                let cfg = zingo_infra_services::validator::ZcashdConfig {
                    zcashd_bin: ZCASHD_BIN.clone(),
                    zcash_cli_bin: ZCASH_CLI_BIN.clone(),
                    rpc_listen_port: Some(zebrad_rpc_listen_port),
                    activation_heights: zingo_infra_services::network::ActivationHeights::default(),
                    miner_address: Some(REG_O_ADDR_FROM_ABANDONART),
                    chain_cache: chain_cache.clone(),
                };
                ValidatorConfig::ZcashdConfig(cfg)
            }
            ValidatorKind::Zebrad => {
                let cfg = zingo_infra_services::validator::ZebradConfig {
                    zebrad_bin: ZEBRAD_BIN.clone(),
                    network_listen_port: None,
                    rpc_listen_port: Some(zebrad_rpc_listen_port),
                    indexer_listen_port: Some(zebrad_grpc_listen_port),
                    activation_heights: zingo_infra_services::network::ActivationHeights::default(),
                    miner_address: zingo_infra_services::validator::ZEBRAD_DEFAULT_MINER,
                    chain_cache: chain_cache.clone(),
                    network,
                };
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

        // Create validator config that will be reused
        let validator_config = ZainoValidatorConfig {
            rpc_address: zebrad_rpc_listen_address,
            indexer_rpc_address: zebrad_grpc_listen_address,
            ..Default::default()
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
                network: match network {
                    Network::Mainnet => zaino_commons::config::Network::Mainnet,
                    Network::Testnet => zaino_commons::config::Network::Testnet,
                    Network::Regtest => zaino_commons::config::Network::Regtest,
                },
                server: zainodlib::config::ServerConfig {
                    json_rpc: if enable_zaino_jsonrpc_server {
                        Some(zaino_commons::config::JsonRpcConfig {
                            listen_address: zaino_json_listen_address,
                            auth: if enable_zaino_jsonrpc_server_cookie_auth {
                                zaino_commons::config::CookieAuth::Enabled {
                                    path: zaino_json_server_cookie_dir
                                        .clone()
                                        .unwrap_or_else(|| PathBuf::from("/tmp/zaino.cookie")),
                                }
                            } else {
                                zaino_commons::config::CookieAuth::Disabled
                            },
                        })
                    } else {
                        None
                    },
                    grpc: zaino_commons::config::GrpcConfig {
                        listen_address: zaino_grpc_listen_address,
                        tls: zaino_commons::config::TlsConfig::Disabled,
                    },
                },
                validator: validator_config.clone(),
                service: zaino_commons::config::ServiceConfig::default(),
                storage: zainodlib::config::StorageConfig {
                    cache: zaino_commons::config::CacheConfig {
                        capacity: None,
                        shard_amount: None,
                    },
                    zaino_database: zaino_commons::config::DatabaseConfig {
                        path: zaino_db_path,
                        size: None,
                    },
                    zebra_database: zaino_commons::config::DatabaseConfig {
                        path: zebra_db_path,
                        size: None,
                    },
                },
                debug: zainodlib::config::DebugConfig {
                    no_sync: zaino_no_sync,
                    no_db: zaino_no_db,
                    slow_sync: false,
                },
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
            let lightclients = build_lightclients(
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
            network: network.into(),
            zebrad_rpc_listen_address,
            zebrad_grpc_listen_address,
            validator_config,
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
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// Get the ValidatorConfig for use by tests.
    pub fn get_validator_config(&self) -> &ZainoValidatorConfig {
        &self.validator_config
    }

    /// Create a JSON RPC connector using the test manager's configuration.
    pub async fn create_json_connector(
        &self,
    ) -> Result<
        zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector,
        zaino_fetch::jsonrpsee::error::TransportError,
    > {
        use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
        // Use the ValidatorConfig's test_and_get_url method to get URL and create connector with auth
        match &self.validator_config.auth {
            AuthMethod::Basic { username, password } => {
                let url = self.validator_config.test_and_get_url().await?;
                JsonRpSeeConnector::new_with_basic_auth(url, username.clone(), password.clone())
            }
            AuthMethod::Cookie { path } => {
                let url = self.validator_config.test_and_get_url().await?;
                JsonRpSeeConnector::new_with_cookie_auth(url, path)
            }
        }
    }

    /// Get a FetchServiceConfig for integration tests.
    pub fn get_fetch_service_config(
        &self,
        zaino_db_path: PathBuf,
        _zebra_db_path: PathBuf,
    ) -> zaino_fetch::config::FetchServiceConfig {
        zaino_fetch::config::FetchServiceConfig {
            validator: self.validator_config.clone(),
            service: ServiceConfig::default(),
            block_cache: BlockCacheConfig {
                cache: CacheConfig::default(),
                database: DatabaseConfig {
                    path: zaino_db_path,
                    size: None,
                },
                network: zaino_commons::config::Network::Regtest, // Tests typically use regtest
                no_sync: true,                                    // Typical for tests
                no_db: true,                                      // Typical for tests
            },
        }
    }

    /// Get a StateServiceConfig for integration tests.
    pub fn get_state_service_config(
        &self,
        zaino_db_path: PathBuf,
        _zebra_db_path: PathBuf,
    ) -> zaino_state::StateServiceConfig {
        zaino_state::StateServiceConfig {
            validator: self.validator_config.clone(),
            service: ServiceConfig::default(),
            block_cache: BlockCacheConfig {
                cache: CacheConfig::default(),
                database: DatabaseConfig {
                    path: zaino_db_path,
                    size: None,
                },
                network: zaino_commons::config::Network::Regtest, // Tests typically use regtest
                no_sync: true,                                    // Typical for tests
                no_db: true,                                      // Typical for tests
            },
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

    use super::*;

    mod zcashd {

        use zingo_infra_testutils::client::build_client;

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
            use zingo_infra_testutils::client::build_client;

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
            use zingo_infra_testutils::client::build_client;

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
