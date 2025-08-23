//! Core validator operations trait.
//!
//! This trait provides the fundamental validator operations that all test managers need,
//! including block generation, address access, and lifecycle management.

use std::net::SocketAddr;
use zaino_commons::config::Network;

/// Core validator operations available to all test managers.
///
/// This trait defines the fundamental validator operations that every test manager
/// needs, regardless of whether it has indexing or wallet capabilities.
pub trait WithValidator {
    /// Get the validator's RPC listen address.
    fn validator_rpc_address(&self) -> SocketAddr;
    
    /// Get the validator's gRPC listen address.
    fn validator_grpc_address(&self) -> SocketAddr;
    
    /// Get the network configuration.
    fn network(&self) -> &Network;

    /// Generate blocks with basic validation.
    async fn generate_blocks(&self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement basic block generation")
    }

    /// Generate blocks with delays to allow sync processes.
    async fn generate_blocks_with_delay(&self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement block generation with sync delays")
    }

    /// Wait for the validator to be ready and responsive.
    async fn wait_for_validator_ready(&self) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement validator readiness check")
    }

    /// Close the validator and clean up resources.
    async fn close(&mut self) {
        todo!("Implement validator cleanup")
    }
}