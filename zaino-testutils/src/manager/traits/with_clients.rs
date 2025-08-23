//! Wallet client operations trait.
//!
//! This trait provides wallet-specific operations for test managers that have
//! lightclients available, eliminating the need for Option unwrapping.

use crate::clients::Clients;
use zingolib::lightclient::LightClient;

/// Wallet client operations for managers that have lightclients.
///
/// This trait is implemented by managers that guarantee the presence of
/// lightclients, eliminating runtime Option unwrapping and providing
/// convenient wallet workflow methods.
pub trait WithClients {
    /// Get direct access to all clients (no Option unwrapping needed).
    fn clients(&self) -> &Clients;

    /// Get the faucet client (convenience method).
    fn faucet(&self) -> &LightClient {
        &self.clients().faucet
    }

    /// Get the recipient client (convenience method).  
    fn recipient(&self) -> &LightClient {
        &self.clients().recipient
    }

    /// Sync all clients and wait for completion.
    async fn sync_clients(&self) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement client synchronization")
    }

    /// Get a faucet address of the specified type.
    async fn get_faucet_address(&self, addr_type: &str) -> String {
        todo!("Implement faucet address generation")
    }

    /// Get a recipient address of the specified type.
    async fn get_recipient_address(&self, addr_type: &str) -> String {
        todo!("Implement recipient address generation")
    }

    /// Common wallet workflow: generate blocks, sync, and shield funds.
    /// 
    /// This combines the common pattern of generating blocks for mining rewards,
    /// syncing the faucet, and shielding the funds for use in tests.
    async fn prepare_for_shielding(&self, blocks: u32) -> Result<(), Box<dyn std::error::Error>>
    where 
        Self: super::WithValidator 
    {
        todo!("Implement prepare_for_shielding workflow")
    }
}