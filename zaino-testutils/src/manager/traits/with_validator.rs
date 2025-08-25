//! Core validator operations trait.
//!
//! This trait provides the fundamental validator operations that all test managers need,
//! including block generation, address access, and lifecycle management.

use std::net::SocketAddr;
use zaino_commons::config::Network;
use crate::validator::LocalNet;

/// Core validator operations available to all test managers.
///
/// This trait defines the fundamental validator operations that every test manager
/// needs, regardless of whether it has indexing or wallet capabilities.
pub trait WithValidator {
    /// Get access to the LocalNet instance.
    fn local_net(&self) -> &LocalNet;

    /// Get mutable access to the LocalNet instance for cleanup operations.
    fn local_net_mut(&mut self) -> &mut LocalNet;

    /// Get the validator's RPC listen address.
    fn validator_rpc_address(&self) -> SocketAddr;
    
    /// Get the validator's gRPC listen address.
    fn validator_grpc_address(&self) -> SocketAddr;
    
    /// Get the network configuration.
    fn network(&self) -> &Network;

    /// Generate blocks with basic validation.
    async fn generate_blocks(&self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        use crate::validator::Validator as _;
        self.local_net().generate_blocks(count).await?;
        Ok(())
    }

    /// Generate blocks with delays to allow sync processes.
    async fn generate_blocks_with_delay(&self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        // Generate blocks one by one with delays to allow sync processes to catch up
        for _ in 0..count {
            self.generate_blocks(1).await?;
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }
        Ok(())
    }

    /// Wait for the validator to be ready and responsive.
    async fn wait_for_validator_ready(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Basic readiness check - try to generate a single block
        self.generate_blocks(1).await?;
        Ok(())
    }

    /// Close the validator and clean up resources.
    async fn close(&mut self) {
        // Default implementation - stop the validator
        use crate::validator::Validator as _;
        self.local_net_mut().stop();
    }
}