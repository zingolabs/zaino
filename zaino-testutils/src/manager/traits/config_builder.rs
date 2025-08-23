//! Configuration builder traits.
//!
//! This module defines the generic traits used by all test manager builders,
//! providing consistent interfaces for configuration and launching.

use std::path::PathBuf;
use zaino_commons::config::Network;
use crate::validator::ValidatorKind;

/// Common interface for all test manager builders.
///
/// This trait provides the standard builder pattern interface that all
/// specialized builders implement, ensuring consistency across different
/// manager types.
pub trait ConfigurableBuilder: Sized {
    /// The manager type this builder creates.
    type Manager;
    
    /// The configuration type this builder generates.
    type Config: TestConfiguration;

    /// Build the final configuration from builder state.
    fn build_config(&self) -> Self::Config;

    /// Launch the manager from this builder.
    async fn launch(self) -> Result<Self::Manager, Box<dyn std::error::Error>>;

    // Standard builder methods that all builders should implement

    /// Set the validator type (Zebra or Zcashd).
    fn validator(self, kind: ValidatorKind) -> Self;

    /// Set the network type (Regtest, Testnet, Mainnet).
    fn network(self, network: Network) -> Self;

    /// Set a custom chain cache directory.
    fn chain_cache(self, path: PathBuf) -> Self;

    // Convenience methods for common configurations

    /// Use Zebra validator (shortcut for .validator(ValidatorKind::Zebra)).
    fn zebra(self) -> Self { 
        self.validator(ValidatorKind::Zebra) 
    }

    /// Use Zcashd validator (shortcut for .validator(ValidatorKind::Zcashd)).
    fn zcashd(self) -> Self { 
        self.validator(ValidatorKind::Zcashd) 
    }

    /// Use regtest network (shortcut for .network(Network::Regtest)).
    fn regtest(self) -> Self { 
        self.network(Network::Regtest) 
    }

    /// Use testnet network (shortcut for .network(Network::Testnet)).
    fn testnet(self) -> Self { 
        self.network(Network::Testnet) 
    }
}

/// Generic trait for launching any manager from any compatible config.
///
/// This trait allows configs to launch their corresponding managers,
/// providing type safety at compile time about config-manager compatibility.
pub trait LaunchManager<M> {
    /// Launch a manager of type M from this configuration.
    async fn launch_manager(self) -> Result<M, Box<dyn std::error::Error>>;
}

/// Marker trait for test configuration types.
///
/// This trait identifies configuration types that are designed for testing,
/// as opposed to production configuration types.
pub trait TestConfiguration {
    /// Get the network this configuration targets.
    fn network(&self) -> &Network;
    
    /// Get the validator kind this configuration uses.
    fn validator_kind(&self) -> ValidatorKind;
}