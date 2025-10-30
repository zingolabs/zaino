//! Common types and configurations shared across Zaino crates.
//!
//! This crate provides shared configuration types, network abstractions,
//! and common utilities used across the Zaino blockchain indexer ecosystem.

pub mod network;
pub mod service;
pub mod storage;
pub mod validator;

// Re-export commonly used types for convenience
pub use network::Network;
pub use service::ServiceConfig;
pub use storage::{CacheConfig, DatabaseConfig, DatabaseSize, StorageConfig};
pub use validator::ValidatorConfig;
