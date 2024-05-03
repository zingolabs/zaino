//! Utility functions for Nym-Proxy

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
        println!("Nym server listening on: {nym_addr}");

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

/// Serialises gRPC request to a buffer.
pub async fn serialize_request<T: prost::Message>(
    request: &T,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    request.encode(&mut buf)?;
    Ok(buf)
}

/// Decodes gRPC request from a buffer
pub async fn deserialize_response<T: prost::Message + Default>(
    data: &[u8],
) -> Result<T, Box<dyn std::error::Error>> {
    T::decode(data).map_err(Into::into)
}
