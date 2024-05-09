//! Nym-gRPC server implementation.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use nym_sdk::mixnet::{MixnetMessageSender, ReconstructedMessage};
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;
use prost::Message;
use zcash_client_backend::proto::service::RawTransaction;

use zingo_rpc::primitives::NymClient;

/// Wrapper struct for a Nym client.
pub struct NymServer(pub NymClient);

impl NymServer {
    /// Receives and decodes encoded gRPC messages sent over the nym mixnet, processes them, encodes the response.
    /// The encoded response is sent back to the sender using a surb (single use reply block).
    pub async fn serve(
        mut self,
        online: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
        let mut request_in: Vec<ReconstructedMessage> = Vec::new();
        tokio::task::spawn(async move {
            while online.load(Ordering::SeqCst) {
                while let Some(request_nym) = self.0 .0.wait_for_messages().await {
                    if request_nym.is_empty() {
                        continue;
                    }
                    request_in = request_nym;
                    break;
                }
                let request_vu8 = request_in
                    .first()
                    .map(|r| r.message.clone())
                    .ok_or_else(|| "No response received from the nym network".to_string())
                    .unwrap();

                //print request for testing
                println!("request received: {:?}", &request_vu8[..]);
                println!("request length: {}", &request_vu8[..].len());

                let request = RawTransaction::decode(&request_vu8[..]).unwrap();
                let response = NymClient::nym_send_transaction(&request).await.unwrap();
                let mut response_vu8 = Vec::new();
                response.encode(&mut response_vu8).unwrap();

                //print response for testing
                println!("response sent: {:?}", &response_vu8[..]);
                println!("response length: {}", &response_vu8[..].len());

                let return_recipient = AnonymousSenderTag::try_from_base58_string(
                    request_in[0].sender_tag.unwrap().to_base58_string(),
                )
                .unwrap();
                self.0
                     .0
                    .send_reply(return_recipient, response_vu8)
                    .await
                    .unwrap();
            }
            Ok(())
        })
    }
}
