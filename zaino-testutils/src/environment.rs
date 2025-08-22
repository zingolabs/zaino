//! Test environment specifications and builders.

use std::path::PathBuf;
use zaino_commons::config::DebugConfig;
use zainodlib::config::IndexerConfig;


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

// TODO: Implement TestConfigBuilder methods in Phase 2
impl TestConfigBuilder {
    // Backend constructors will go here
    // Type-safe auth methods will go here  
    // Configuration methods will go here
    // Convenience constructors will go here
}



