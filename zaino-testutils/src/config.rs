//! Test configuration builders with type-safe backend selection.

use std::path::PathBuf;
use zaino_commons::config::{
    BackendConfig, CacheConfig, CookieAuth, DatabaseConfig, DebugConfig, GrpcConfig, 
    JsonRpcAuth, JsonRpcConfig, Network, ServiceConfig, StorageConfig, TlsConfig,
    ZcashdAuth, ZebradAuth, ZebraStateConfig, PasswordAuth,
};
use zainodlib::config::{default_ephemeral_cookie_path, IndexerConfig, ServerConfig};


/// Test-specific configuration flags.
#[derive(Debug, Clone)]
pub struct TestingFlags {
    /// Skip blockchain sync.
    pub no_sync: bool,
    /// Skip database persistence.
    pub no_db: bool,
    /// Slower sync for testing.
    pub slow_sync: bool,
}

impl Default for TestingFlags {
    fn default() -> Self {
        Self {
            no_sync: true,  // Default for tests
            no_db: true,    // Default for tests
            slow_sync: false,
        }
    }
}

impl From<TestingFlags> for DebugConfig {
    fn from(flags: TestingFlags) -> Self {
        Self {
            no_sync: flags.no_sync,
            no_db: flags.no_db,
            slow_sync: flags.slow_sync,
        }
    }
}

/// Test configuration builder with type-safe backend selection.
///
/// Uses IndexerConfig internally while providing test-friendly APIs
/// that prevent invalid combinations (e.g., State mode with Zcashd).
#[derive(Debug, Clone)]
pub struct TestConfigBuilder {
    /// Complete indexer configuration.
    config: IndexerConfig,
    /// Enable zingolib lightclients.
    enable_lightclients: bool,
    /// Optional chain cache directory for validator.
    chain_cache: Option<PathBuf>,
}

impl TestConfigBuilder {
    /// Create a local Zebra configuration (StateService with direct state access).
    pub fn local_zebra() -> Self {
        Self {
            config: IndexerConfig {
                network: Network::Regtest,
                server: ServerConfig::default(),
                backend: BackendConfig::LocalZebra {
                    rpc_address: "127.0.0.1:0".parse().unwrap(), // Placeholder port
                    auth: ZebradAuth::Disabled,
                    zebra_state: ZebraStateConfig::default(),
                    indexer_rpc_address: "127.0.0.1:0".parse().unwrap(), // Placeholder port
                    zebra_database: DatabaseConfig::default(),
                },
                service: ServiceConfig::default(),
                storage: StorageConfig::default(),
                debug: DebugConfig::default(),
            },
            enable_lightclients: false,
            chain_cache: None,
        }
    }

    /// Create a remote Zebra configuration (FetchService with JSON-RPC).
    pub fn remote_zebra() -> Self {
        Self {
            config: IndexerConfig {
                network: Network::Regtest,
                server: ServerConfig::default(),
                backend: BackendConfig::RemoteZebra {
                    rpc_address: "127.0.0.1:0".parse().unwrap(), // Placeholder port
                    auth: ZebradAuth::Disabled,
                },
                service: ServiceConfig::default(),
                storage: StorageConfig::default(),
                debug: DebugConfig::default(),
            },
            enable_lightclients: false,
            chain_cache: None,
        }
    }

    /// Create a remote Zcashd configuration (FetchService with JSON-RPC).
    pub fn remote_zcashd() -> Self {
        Self {
            config: IndexerConfig {
                network: Network::Regtest,
                server: ServerConfig::default(),
                backend: BackendConfig::RemoteZcashd {
                    rpc_address: "127.0.0.1:0".parse().unwrap(), // Placeholder port
                    auth: ZcashdAuth::Disabled,
                },
                service: ServiceConfig::default(),
                storage: StorageConfig::default(),
                debug: DebugConfig::default(),
            },
            enable_lightclients: false,
            chain_cache: None,
        }
    }

    /// Set Zebra cookie authentication (only valid for Zebra backends).
    pub fn with_zebra_cookie_auth(mut self, cookie_path: PathBuf) -> Self {
        match &mut self.config.backend {
            BackendConfig::LocalZebra { auth, .. } 
            | BackendConfig::RemoteZebra { auth, .. } => {
                *auth = ZebradAuth::Cookie(CookieAuth { path: cookie_path });
            }
            _ => panic!("Cookie auth only valid for Zebra backends"),
        }
        self
    }

    /// Set Zcashd password authentication (only valid for Zcashd backends).
    pub fn with_zcashd_password_auth(mut self, username: String, password: String) -> Self {
        match &mut self.config.backend {
            BackendConfig::RemoteZcashd { auth, .. } => {
                *auth = ZcashdAuth::Password(PasswordAuth::new(username, password));
            }
            _ => panic!("Password auth only valid for Zcashd backends"),
        }
        self
    }

    /// Set the network (Mainnet, Testnet, Regtest).
    pub fn with_network(mut self, network: Network) -> Self {
        self.config.network = network;
        self
    }

    /// Set chain cache directory for the validator.
    pub fn with_chain_cache(mut self, cache_path: PathBuf) -> Self {
        self.chain_cache = Some(cache_path);
        self
    }

    /// Enable JSON-RPC server on the indexer.
    pub fn with_json_server(mut self) -> Self {
        self.config.server.json_rpc = Some(JsonRpcConfig {
            listen_address: "127.0.0.1:0".parse().unwrap(), // Placeholder port
            auth: JsonRpcAuth::Disabled,
        });
        self
    }

    /// Enable lightclients for testing.
    pub fn with_lightclients(mut self) -> Self {
        self.enable_lightclients = true;
        self
    }

    /// Enable sync and database persistence (disable no_sync and no_db flags).
    pub fn with_sync_and_db(mut self) -> Self {
        self.config.debug.no_sync = false;
        self.config.debug.no_db = false;
        self
    }

    /// Set JSON server authentication.
    pub fn with_server_auth(mut self, auth: JsonRpcAuth) -> Self {
        if let Some(ref mut json_rpc) = self.config.server.json_rpc {
            json_rpc.auth = auth;
        }
        self
    }

    /// Set cookie authentication for both validator and server.
    pub fn with_cookie_auth(mut self, cookie_path: PathBuf) -> Self {
        // Set validator auth
        self = self.with_zebra_cookie_auth(cookie_path.clone());
        
        // Set server auth if JSON server is enabled
        if self.config.server.json_rpc.is_some() {
            self = self.with_server_auth(JsonRpcAuth::Cookie(CookieAuth { path: cookie_path }));
        }
        self
    }

    /// Advanced customization escape hatch.
    pub fn customize_config<F>(mut self, f: F) -> Self 
    where F: FnOnce(&mut IndexerConfig) {
        f(&mut self.config);
        self
    }

    /// Convenience: Full stack local Zebra environment.
    pub fn full_stack_local_zebra() -> Self {
        Self::local_zebra()
            .with_lightclients()
    }

    /// Convenience: JSON server test environment with remote Zebra.
    pub fn json_server_tests() -> Self {
        Self::remote_zebra()
            .with_json_server()
    }

    /// Convenience: JSON server tests with cookie auth.
    pub fn json_server_tests_with_auth() -> Self {
        Self::remote_zebra()
            .with_json_server()
            .with_cookie_auth(default_ephemeral_cookie_path())
    }

    /// Convenience: Basic tests with remote Zcashd.
    pub fn basic_tests_remote_zcashd() -> Self {
        Self::remote_zcashd()
            .with_sync_and_db()
    }

    /// Convenience: State service tests with local Zebra.
    pub fn state_tests() -> Self {
        Self::local_zebra()
            .with_sync_and_db()
    }

    /// Convenience: Wallet integration tests.
    pub fn wallet_tests_local_zebra() -> Self {
        Self::local_zebra()
            .with_lightclients()
    }

    /// Convenience: Wallet integration tests with remote Zcashd.
    pub fn wallet_tests_remote_zcashd() -> Self {
        Self::remote_zcashd()
            .with_lightclients()
    }

    // Accessors for TestManager to use
    pub(crate) fn indexer_config(&self) -> &IndexerConfig {
        &self.config
    }

    pub(crate) fn enable_lightclients(&self) -> bool {
        self.enable_lightclients
    }

    pub(crate) fn chain_cache(&self) -> Option<&PathBuf> {
        self.chain_cache.as_ref()
    }

    pub(crate) fn into_parts(self) -> (IndexerConfig, bool, Option<PathBuf>) {
        (self.config, self.enable_lightclients, self.chain_cache)
    }
}



