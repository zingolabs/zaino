//! Nym client functionality.
//!
//! TODO: - Add NymClientError error type and rewrite functions to return <Result<(), NymClientError>>.

use nym_sdk::mixnet::{
    MixnetClientBuilder, MixnetMessageSender, Recipient, ReconstructedMessage, StoragePaths,
};
use std::path::PathBuf;

use crate::primitives::NymClient;

impl NymClient {
    /// Spawns a nym client and connects to the mixnet.
    pub async fn nym_spawn(str_path: &str) -> Self {
        //nym_bin_common::logging::setup_logging();
        let client = MixnetClientBuilder::new_with_default_storage(
            StoragePaths::new_from_dir(PathBuf::from(str_path)).unwrap(),
        )
        .await
        .unwrap()
        .build()
        .unwrap()
        .connect_to_mixnet()
        .await
        .unwrap();

        let nym_addr = client.nym_address().to_string();
        println!("@zingoproxyd[nym]: Nym server listening on: {nym_addr}.");

        Self(client)
    }

    /// Forwards an encoded gRPC request over the nym mixnet to the nym address specified and waits for the response.
    pub async fn nym_forward(
        &mut self,
        recipient_address: &str,
        message: Vec<u8>,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let recipient: Recipient =
            Recipient::try_from_base58_string(recipient_address.to_string()).unwrap();
        self.0.send_plain_message(recipient, message).await.unwrap();

        let mut nym_response: Vec<ReconstructedMessage> = Vec::new();
        while let Some(response_in) = self.0.wait_for_messages().await {
            if response_in.is_empty() {
                continue;
            }
            nym_response = response_in;
            break;
        }
        let response_out = nym_response
            .first()
            .map(|r| r.message.clone())
            .ok_or_else(|| "No response received from the nym network".to_string())?;
        Ok(response_out)
    }

    /// Closes the nym client.
    pub async fn nym_close(self) {
        self.0.disconnect().await;
    }
}
