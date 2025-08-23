//! Specialized test manager implementations.
//!
//! This module contains the concrete implementations of specialized test managers
//! for different testing scenarios, each implementing exactly the traits they need.

pub mod service;
pub mod wallet;
pub mod json_server;

// Re-export all manager types and builders for convenience
pub use service::{ServiceTestManager, ServiceTestsBuilder};
pub use wallet::{WalletTestManager, WalletTestsBuilder};
pub use json_server::{JsonServerTestManager, JsonServerTestsBuilder};