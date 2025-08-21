//! Test environment specifications and builders.

use std::{path::PathBuf, time::Duration};
use zaino_commons::config::{
    CookieAuth, DebugConfig, JsonRpcAuth, Network, ServiceConfig, StorageConfig,
};
use zainodlib::config::{default_ephemeral_cookie_path, IndexerConfig};

#[derive(Debug, PartialEq, Clone, Copy)]
/// Represents the type of validator to launch.
pub enum ValidatorKind {
    /// Zcashd.
    Zcashd,
    /// Zebrd.
    Zebrd,
}

#[derive(Debug, PartialEq, Clone, Copy)]
/// How the indexer connects to the validator.
pub enum BackendMode {
    /// JSON-RPC connection to validator.
    Fetch,
    /// Direct zebra state access (zebra only).
    State,
}

/// What validator to run and how.
#[derive(Debug, Clone)]
pub struct ValidatorSpec {
    /// Type of validator (zcashd vs zebrd).
    pub kind: ValidatorKind,
    /// Network configuration.
    pub network: Network,
    /// Optional chain cache directory.
    pub chain_cache: Option<PathBuf>,
}

/// What indexing service to run.
#[derive(Debug, Clone)]
pub struct IndexerSpec {
    /// How indexer connects to validator.
    pub backend_mode: BackendMode,
    /// Enable JSON-RPC server.
    pub enable_json_server: bool,
    /// Test-specific flags.
    pub testing_flags: TestingFlags,
}

/// Test client configuration.
#[derive(Debug, Clone)]
pub struct ClientSpec {
    /// Enable zingolib lightclients.
    pub enable_lightclients: bool,
}

/// Authentication between services.
#[derive(Debug, Clone)]
pub struct AuthSpec {
    /// How to authenticate to validator.
    pub validator_auth: JsonRpcAuth,
    /// How to authenticate to zaino's JSON server.
    pub server_auth: JsonRpcAuth,
}

impl Default for AuthSpec {
    fn default() -> Self {
        Self {
            validator_auth: JsonRpcAuth::Disabled,
            server_auth: JsonRpcAuth::Disabled,
        }
    }
}

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

/// Test environment topology specification.
pub struct TestEnvironment {
    /// Validator specification.
    pub validator: ValidatorSpec,
    /// Optional indexer specification.
    pub indexer: Option<IndexerSpec>,
    /// Optional client specification.
    pub clients: Option<ClientSpec>,
    /// Authentication specification.
    pub auth: AuthSpec,
    /// Config customizers that modify the final IndexerConfig.
    pub indexer_customizers: Vec<Box<dyn Fn(&mut IndexerConfig) + Send + Sync + 'static>>,
}

impl std::fmt::Debug for TestEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestEnvironment")
            .field("validator", &self.validator)
            .field("indexer", &self.indexer)
            .field("clients", &self.clients)
            .field("auth", &self.auth)
            .field("indexer_customizers", &format!("{} customizers", self.indexer_customizers.len()))
            .finish()
    }
}

impl Clone for TestEnvironment {
    fn clone(&self) -> Self {
        Self {
            validator: self.validator.clone(),
            indexer: self.indexer.clone(),
            clients: self.clients.clone(),
            auth: self.auth.clone(),
            indexer_customizers: Vec::new(), // Can't clone function pointers
        }
    }
}

impl TestEnvironment {
    /// Minimal validator-only environment.
    pub fn validator_only(kind: ValidatorKind) -> Self {
        Self {
            validator: ValidatorSpec {
                kind,
                network: Network::Regtest,
                chain_cache: None,
            },
            indexer: None,
            clients: None,
            auth: AuthSpec::default(),
            indexer_customizers: Vec::new(),
        }
    }

    /// Full stack test environment (common pattern).
    pub fn full_stack(kind: ValidatorKind, backend_mode: BackendMode) -> Self {
        Self::validator_only(kind)
            .with_indexer(backend_mode)
            .with_clients()
    }

    /// JSON server test environment (common pattern).
    pub fn json_server_tests(kind: ValidatorKind, enable_cookie_auth: bool) -> Self {
        let mut env = Self::validator_only(kind)
            .with_indexer(BackendMode::Fetch)
            .with_json_server();

        if enable_cookie_auth {
            env = env.with_cookie_auth(default_ephemeral_cookie_path());
        }

        env
    }

    /// Basic test environment.
    pub fn basic_tests(kind: ValidatorKind, backend_mode: BackendMode) -> Self {
        Self::validator_only(kind)
            .with_indexer(backend_mode)
            .with_sync_and_db()
    }

    /// Chain cache test environment.
    pub fn chain_cache_tests(kind: ValidatorKind, cache_path: PathBuf) -> Self {
        Self::validator_only(kind)
            .with_indexer(BackendMode::Fetch)
            .with_chain_cache(cache_path)
    }

    /// State service test environment.
    pub fn state_tests(kind: ValidatorKind) -> Self {
        Self::validator_only(kind)
            .with_indexer(BackendMode::State)
            .with_sync_and_db()
    }

    /// Wallet integration test environment.
    pub fn wallet_tests(kind: ValidatorKind, backend_mode: BackendMode) -> Self {
        Self::validator_only(kind)
            .with_indexer(backend_mode)
            .with_clients()
    }

    // Builder methods
    /// Add indexer to environment.
    pub fn with_indexer(mut self, backend_mode: BackendMode) -> Self {
        self.indexer = Some(IndexerSpec {
            backend_mode,
            enable_json_server: false,
            testing_flags: TestingFlags::default(),
        });
        self
    }

    /// Enable JSON-RPC server on indexer.
    pub fn with_json_server(mut self) -> Self {
        if let Some(ref mut indexer) = self.indexer {
            indexer.enable_json_server = true;
        }
        self
    }

    /// Add lightclients to environment.
    pub fn with_clients(mut self) -> Self {
        self.clients = Some(ClientSpec {
            enable_lightclients: true,
        });
        self
    }

    /// Set chain cache path.
    pub fn with_chain_cache(mut self, cache_path: PathBuf) -> Self {
        self.validator.chain_cache = Some(cache_path);
        self
    }

    /// Set network.
    pub fn with_network(mut self, network: Network) -> Self {
        self.validator.network = network;
        self
    }

    /// Enable sync and database (disable testing flags).
    pub fn with_sync_and_db(mut self) -> Self {
        if let Some(ref mut indexer) = self.indexer {
            indexer.testing_flags.no_sync = false;
            indexer.testing_flags.no_db = false;
        }
        self
    }

    /// Set authentication configuration.
    pub fn with_auth(mut self, validator_auth: JsonRpcAuth, server_auth: JsonRpcAuth) -> Self {
        self.auth = AuthSpec {
            validator_auth,
            server_auth,
        };
        self
    }

    /// Set cookie authentication.
    pub fn with_cookie_auth(mut self, cookie_path: PathBuf) -> Self {
        self.auth.validator_auth = JsonRpcAuth::Cookie(CookieAuth {
            path: cookie_path.clone(),
        });
        self.auth.server_auth = JsonRpcAuth::Cookie(CookieAuth {
            path: cookie_path,
        });
        self
    }

    // Config customization methods (Approach 2.5)
    /// Modify the IndexerConfig that would be built.
    pub fn customize_indexer<F>(mut self, customizer: F) -> Self
    where
        F: Fn(&mut IndexerConfig) + Send + Sync + 'static,
    {
        self.indexer_customizers.push(Box::new(customizer));
        self
    }

    /// Modify just the storage configuration.
    pub fn customize_storage<F>(self, customizer: F) -> Self
    where
        F: Fn(&mut StorageConfig) + Send + Sync + 'static,
    {
        self.customize_indexer(move |config| customizer(&mut config.storage))
    }

    /// Modify just the service configuration.
    pub fn customize_service<F>(self, customizer: F) -> Self
    where
        F: Fn(&mut ServiceConfig) + Send + Sync + 'static,
    {
        self.customize_indexer(move |config| customizer(&mut config.service))
    }

    /// Common customization: set database size.
    pub fn with_database_size(self, size_bytes: usize) -> Self {
        self.customize_storage(move |storage| storage.database.size = Some(size_bytes))
    }

    /// Common customization: set cache capacity.
    pub fn with_cache_capacity(self, capacity: usize) -> Self {
        self.customize_storage(move |storage| storage.cache.capacity = Some(capacity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_environment_builder() {
        let env = TestEnvironment::validator_only(ValidatorKind::Zebrd)
            .with_indexer(BackendMode::Fetch)
            .with_clients()
            .with_database_size(1024 * 1024);

        assert_eq!(env.validator.kind, ValidatorKind::Zebrd);
        assert!(env.indexer.is_some());
        assert!(env.clients.is_some());
        assert_eq!(env.indexer_customizers.len(), 1);
    }

    #[tokio::test]
    async fn test_json_server_environment() {
        let env = TestEnvironment::json_server_tests(ValidatorKind::Zcashd, false);

        assert_eq!(env.validator.kind, ValidatorKind::Zcashd);
        assert!(env.indexer.is_some());
        assert!(env.indexer.as_ref().unwrap().enable_json_server);
        assert_eq!(
            env.indexer.as_ref().unwrap().backend_mode,
            BackendMode::Fetch
        );
    }
}