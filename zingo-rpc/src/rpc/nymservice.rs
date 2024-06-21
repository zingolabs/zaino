//! Lightwallet service RPC Nym implementations.

use std::sync::Arc;

use http::Uri;
use tonic::Request;
use zcash_client_backend::proto::service::{RawTransaction, SendResponse};
use zingo_netutils::GrpcConnector;

use crate::primitives::NymClient;

impl NymClient {
    /// Forwards the recieved send_transaction request on to a Lightwalletd and returns the response.
    pub async fn nym_send_transaction(
        request: &RawTransaction,
    ) -> Result<SendResponse, Box<dyn std::error::Error>> {
        // TODO: Expose zproxy_port to point to actual zproxy listen port.
        let zproxy_port = 8080;
        let zproxy_uri = Uri::builder()
            .scheme("http")
            .authority(format!("localhost:{zproxy_port}"))
            .path_and_query("/")
            .build()
            .unwrap();
        let client = Arc::new(GrpcConnector::new(zproxy_uri));

        let mut cmp_client = client
            .get_client()
            .await
            .map_err(|e| format!("Error getting client: {:?}", e))?;
        let grpc_request = Request::new(request.clone());

        let response = cmp_client
            .send_transaction(grpc_request)
            .await
            .map_err(|e| format!("Send Error: {}", e))?;
        Ok(response.into_inner())
    }
}
