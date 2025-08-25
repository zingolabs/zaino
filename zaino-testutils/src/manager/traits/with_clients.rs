//! Wallet client operations trait.
//!
//! This trait provides wallet-specific operations for test managers that have
//! lightclients available, eliminating the need for Option unwrapping.

use super::WithValidator;
use crate::clients::{ClientAddressType, Clients};
use zingolib::lightclient::LightClient;

/// Wallet client operations for managers that have lightclients.
///
/// This trait is implemented by managers that guarantee the presence of
/// lightclients, eliminating runtime Option unwrapping and providing
/// convenient wallet workflow methods.
pub trait WithClients: WithValidator {
    /// Get direct access to all clients (no Option unwrapping needed).
    fn clients(&self) -> &Clients;
    fn clients_mut(&mut self) -> &mut Clients;

    /// Get the faucet client (convenience method).
    fn faucet(&mut self) -> &mut LightClient {
        &mut self.clients_mut().faucet
    }

    /// Get the recipient client (convenience method).  
    fn recipient(&mut self) -> &mut LightClient {
        &mut self.clients_mut().recipient
    }

    /// Sync all clients and wait for completion.
    async fn sync_clients(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.faucet().sync_and_await().await?;
        self.recipient().sync_and_await().await?;
        Ok(())
    }

    /// Get a faucet address of the specified type.
    async fn get_faucet_address(&mut self, addr_type: ClientAddressType) -> String {
        self.clients_mut().get_faucet_address(addr_type).await
    }

    /// Get a recipient address of the specified type.
    async fn get_recipient_address(&mut self, addr_type: ClientAddressType) -> String {
        self.clients_mut().get_recipient_address(addr_type).await
    }

    /// Common wallet workflow: generate blocks, sync, and shield funds.
    ///
    /// This combines the common pattern of generating blocks for mining rewards,
    /// syncing the faucet, and shielding the funds for use in tests.
    async fn prepare_for_shielding(&mut self, blocks: u32) -> Result<(), Box<dyn std::error::Error>>
    where
        Self: super::WithValidator,
    {
        self.generate_blocks_with_delay(blocks).await?;
        self.faucet().sync_and_await().await?;
        self.faucet().quick_shield().await?;
        self.generate_blocks_with_delay(1).await?;
        self.faucet().sync_and_await().await?;
        Ok(())
    }
}
