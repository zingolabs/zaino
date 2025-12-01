//! Common types and configurations shared across Zaino crates.
//!
//! This crate provides shared configuration types, network abstractions,
//! and common utilities used across the Zaino blockchain indexer ecosystem.

pub mod config;

// Re-export commonly used config types at crate root for backward compatibility.
// This allows existing code using `use zaino_common::Network` to continue working.
pub use config::network::{ActivationHeights, Network, ZEBRAD_DEFAULT_ACTIVATION_HEIGHTS};
pub use config::service::ServiceConfig;
pub use config::storage::{CacheConfig, DatabaseConfig, DatabaseSize, StorageConfig};
pub use config::validator::ValidatorConfig;

// Keep submodule access available for more specific imports if needed
pub use config::network;
pub use config::service;
pub use config::storage;
pub use config::validator;
