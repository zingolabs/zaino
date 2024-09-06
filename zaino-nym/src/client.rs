//! Nym client functionality.

use nym_sdk::mixnet::{
    MixnetClient, MixnetClientBuilder, MixnetMessageSender, Recipient, ReconstructedMessage,
    StoragePaths,
};
use std::path::PathBuf;

use crate::error::NymError;

/// Wrapper struct for a Nym client.
pub struct NymClient {
    /// Nym SDK Client.
    pub client: MixnetClient,
    /// Nym client address.
    pub addr: String,
}

impl NymClient {
    /// Spawns a nym client and connects to the mixnet.
    pub async fn spawn(str_path: &str) -> Result<Self, NymError> {
        //nym_bin_common::logging::setup_logging();
        let client = MixnetClientBuilder::new_with_default_storage(StoragePaths::new_from_dir(
            PathBuf::from(str_path),
        )?)
        .await?
        .build()?
        .connect_to_mixnet()
        .await?;
        let addr = client.nym_address().to_string();
        Ok(Self { client, addr })
    }

    /// Forwards an encoded gRPC request over the nym mixnet to the nym address specified and waits for the response.
    ///
    /// TODO: Add timout for waiting for response.
    pub async fn send(
        &mut self,
        recipient_address: &str,
        message: Vec<u8>,
    ) -> Result<Vec<u8>, NymError> {
        // Box<dyn std::error::Error>> {
        let recipient: Recipient =
            Recipient::try_from_base58_string(recipient_address.to_string())?;
        self.client.send_plain_message(recipient, message).await?;

        let mut nym_response: Vec<ReconstructedMessage> = Vec::new();
        while let Some(response_in) = self.client.wait_for_messages().await {
            if response_in.is_empty() {
                continue;
            }
            nym_response = response_in;
            break;
        }
        let response_out = nym_response
            .first()
            .map(|r| r.message.clone())
            .ok_or_else(|| {
                NymError::ConnectionError("No response received from the nym network".to_string())
            })?;
        Ok(response_out)
    }

    /// Closes the nym client.
    pub async fn close(self) {
        self.client.disconnect().await;
    }
}
