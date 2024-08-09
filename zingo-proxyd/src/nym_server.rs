//! Nym-gRPC server implementation.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use nym_sdk::mixnet::{MixnetMessageSender, ReconstructedMessage};
use nym_sphinx_anonymous_replies::requests::AnonymousSenderTag;

use zingo_rpc::{nym::client::NymClient, rpc::GrpcClient, server::request::ZingoProxyRequest};

/// Wrapper struct for a Nym client.
pub struct NymServer {
    /// NymClient data
    pub nym_client: NymClient,
    /// Nym Address
    pub nym_addr: String,
    /// Represents the Online status of the gRPC server.
    pub online: Arc<AtomicBool>,
}

impl NymServer {
    /// Receives and decodes encoded gRPC messages sent over the nym mixnet, processes them, encodes the response.
    /// The encoded response is sent back to the sender using a surb (single use reply block).
    pub async fn serve(mut self) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
        let mut request_in: Vec<ReconstructedMessage> = Vec::new();
        tokio::task::spawn(async move {
            // NOTE: This interval may need to be reduced or removed / moved once scale testing begins.
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
            // NOTE: the following should be removed with the addition of the queue and worker pool.
            let lwd_port = 8080;
            let zebrad_port = 18232;
            let proxy_client = GrpcClient {
                lightwalletd_uri: http::Uri::builder()
                    .scheme("http")
                    .authority(format!("localhost:{lwd_port}"))
                    .path_and_query("/")
                    .build()
                    .unwrap(),
                zebrad_uri: http::Uri::builder()
                    .scheme("http")
                    .authority(format!("localhost:{zebrad_port}"))
                    .path_and_query("/")
                    .build()
                    .unwrap(),
                online: self.online.clone(),
            };
            while self.online.load(Ordering::SeqCst) {
                // --- wait for request.
                while let Some(request_nym) = self.nym_client.client.wait_for_messages().await {
                    if request_nym.is_empty() {
                        interval.tick().await;
                        if !self.online.load(Ordering::SeqCst) {
                            println!("Nym server shutting down.");
                            return Ok(());
                        }
                        continue;
                    }
                    request_in = request_nym;
                    break;
                }

                // --- decode request
                let request_vu8 = request_in
                    .first()
                    .map(|r| r.message.clone())
                    .ok_or_else(|| "No response received from the nym network".to_string())
                    .unwrap();
                // --- fetch recipient address
                let return_recipient = AnonymousSenderTag::try_from_base58_string(
                    request_in[0].sender_tag.unwrap().to_base58_string(),
                )
                .unwrap();
                // --- build ZingoProxyRequest
                let zingo_proxy_request =
                    ZingoProxyRequest::new_from_nym(return_recipient, request_vu8.as_ref())
                        .unwrap();

                // print request for testing
                // println!(
                //     "@zingoproxyd[nym][TEST]: ZingoProxyRequest recieved: {:?}.",
                //     zingo_proxy_request
                // );

                // --- process request
                // NOTE: when the queue is added requests will not be processed here but by the queue!
                let response: Vec<u8>;
                match zingo_proxy_request {
                    ZingoProxyRequest::NymServerRequest(request) => {
                        response = proxy_client.process_nym_request(&request).await.unwrap();
                    }
                    _ => {
                        todo!()
                    }
                }

                // print response for testing
                // println!(
                //     "@zingoproxyd[nym][TEST]: Response sent: {:?}.",
                //     &response[..],
                // );

                // --- send response
                self.nym_client
                    .client
                    .send_reply(return_recipient, response)
                    .await
                    .unwrap();
            }
            // Why print this?
            println!("Nym server shutting down.");
            Ok(())
        })
    }

    /// Returns a new NymServer Inatanse
    pub async fn spawn(nym_conf_path: &str, online: Arc<AtomicBool>) -> Self {
        let nym_client = NymClient::spawn(nym_conf_path).await.unwrap();
        let nym_addr = nym_client.client.nym_address().to_string();
        NymServer {
            nym_client,
            nym_addr,
            online,
        }
    }
}
