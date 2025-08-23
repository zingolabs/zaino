//! Trait system for composable test manager capabilities.
//!
//! This module contains the trait-based architecture that allows different
//! test managers to implement exactly the capabilities they need, providing
//! type safety and eliminating runtime Option unwrapping.

pub mod with_validator;
pub mod with_clients;
pub mod with_indexer;
pub mod with_service_factories;
pub mod config_builder;

// Re-export all traits for convenience
pub use with_validator::WithValidator;
pub use with_clients::WithClients;
pub use with_indexer::WithIndexer;
pub use with_service_factories::WithServiceFactories;
pub use config_builder::{ConfigurableBuilder, LaunchManager, TestConfiguration};