//! Test manager orchestration and specialized managers.
//!
//! This module provides the new trait-based architecture for test management,
//! replacing the monolithic TestManager with specialized managers for different
//! test scenarios.

pub mod factories;
pub mod tests;
pub mod traits;

use self::tests::{
    json_server::{JsonServerTestManager, JsonServerTestsBuilder},
    service::{ServiceTestManager, ServiceTestsBuilder},
    wallet::{WalletTestManager, WalletTestsBuilder},
};

/// Public facade for creating specialized test managers.
pub struct TestManagerBuilder;

impl TestManagerBuilder {
    /// Zero-config shortcut for service tests (validator + service factories).
    pub async fn for_service_tests() -> Result<ServiceTestManager, Box<dyn std::error::Error>> {
        todo!("Implement zero-config service test manager")
    }

    /// Zero-config shortcut for wallet tests (validator + indexer + clients).
    pub async fn for_wallet_tests() -> Result<WalletTestManager, Box<dyn std::error::Error>> {
        todo!("Implement zero-config wallet test manager")
    }

    /// Zero-config shortcut for JSON server tests (validator + indexer + JSON server).
    pub async fn for_json_server_tests() -> Result<JsonServerTestManager, Box<dyn std::error::Error>>
    {
        todo!("Implement zero-config JSON server test manager")
    }

    /// Customizable builder for service tests.
    pub fn service_tests() -> ServiceTestsBuilder {
        todo!("Create service tests builder")
    }

    /// Customizable builder for wallet tests.
    pub fn wallet_tests() -> WalletTestsBuilder {
        todo!("Create wallet tests builder")
    }

    /// Customizable builder for JSON server tests.
    pub fn json_server_tests() -> JsonServerTestsBuilder {
        todo!("Create JSON server tests builder")
    }
}
