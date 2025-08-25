//! Service creation factories trait.
//!
//! This trait provides factory methods for creating common services with
//! sensible defaults, eliminating the massive boilerplate typically required
//! for service creation in integration tests.

use crate::manager::factories::{BlockCacheBuilder, FetchServiceBuilder, StateServiceBuilder};
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;

/// Service creation with sensible defaults.
///
/// This trait provides pre-configured builders for common services, using
/// the test manager's validator configuration to provide appropriate defaults.
/// This eliminates the 40+ line service creation boilerplate.
pub trait WithServiceFactories: super::WithValidator {
    /// Create a FetchService builder with pre-configured validator connection.
    ///
    /// The builder comes pre-configured with:
    /// - Validator RPC address
    /// - Network parameters (including regtest activation heights)
    /// - Authentication (if available)
    /// - Default data directory
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        todo!("Create pre-configured FetchServiceBuilder")
    }

    /// Create a StateService builder with pre-configured validator connection.
    ///
    /// The builder comes pre-configured with:
    /// - Validator state service connection
    /// - Network parameters
    /// - Default cache directory
    /// - Performance-optimized defaults
    fn create_state_service(&self) -> StateServiceBuilder {
        todo!("Create pre-configured StateServiceBuilder")
    }

    /// Create a JSON-RPC connector with authentication.
    ///
    /// Returns a ready-to-use connector with:
    /// - Validator RPC address
    /// - Basic authentication configured
    /// - Connection tested and verified
    fn create_json_connector(&self) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        todo!("Create authenticated JSON-RPC connector")
    }

    /// Create a BlockCache builder with performance defaults.
    ///
    /// The builder comes pre-configured with:
    /// - Network parameters
    /// - Performance-optimized cache settings
    /// - Temporary database directory
    /// - Sync and DB options
    fn create_block_cache(&self) -> BlockCacheBuilder {
        todo!("Create pre-configured BlockCacheBuilder")
    }
}
