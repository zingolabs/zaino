//! Specialized test manager implementations.
//!
//! This module contains the concrete implementations of specialized test managers
//! for different testing scenarios, each implementing exactly the traits they need.

pub mod json_server;
pub mod json_server_comparison;
pub mod service;
pub mod state_service_comparison;
pub mod wallet;

// Re-export all manager types and builders for convenience
pub use json_server::{JsonServerTestManager, JsonServerTestsBuilder};
pub use json_server_comparison::{JsonServerComparisonTestManager, JsonServerComparisonTestsBuilder};
pub use service::{ServiceTestManager, ServiceTestsBuilder};
pub use state_service_comparison::{StateServiceComparisonTestManager, StateServiceComparisonTestsBuilder};
pub use wallet::{WalletTestManager, WalletTestsBuilder};
