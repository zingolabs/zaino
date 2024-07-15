//! Lightwallet service RPC Nym implementations.

use std::sync::Arc;

use http::Uri;
use prost::Message;
use tonic::Request;
use zcash_client_backend::proto::service::{Empty, LightdInfo, RawTransaction, SendResponse};
use zingo_netutils::GrpcConnector;

use crate::{primitives::NymClient, queue::request::ZingoProxyRequest};

impl NymClient {
    /// Handles nym_request based on method field.
    /// TODO: use generic output to return correct return type???
    /// TODO: handle GrpcRequest variant by returning error
    pub async fn process_request<T: prost::Message>(
        request: ZingoProxyRequest,
    ) -> Result<T, Box<dyn std::error::Error>> {
        match request.method().as_str() {
            "get_lightd_info" => {
                let input = Empty::decode(&request.body()[..]).unwrap();
                // Ok(Self::nym_get_lightd_info(&input).await)
                todo!()
            }
            "send_transaction" => {
                let input = RawTransaction::decode(&request.body()[..]).unwrap();
                // Ok(Self::nym_send_transaction(&input).await)
                todo!()
            }
            _ => {
                todo!()
            } // Err,
        }
    }

    /// Forwards the recieved send_transaction request on to a Lightwalletd and returns the response.
    ///
    /// TODO: Process request here / call service function directly instead of forwarding request on to gRPC server, as this will queue request twice.
    pub async fn nym_get_lightd_info(
        request: &Empty,
    ) -> Result<LightdInfo, Box<dyn std::error::Error>> {
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
            .get_lightd_info(grpc_request)
            .await
            .map_err(|e| format!("Send Error: {}", e))?;
        Ok(response.into_inner())
    }

    /// Forwards the recieved send_transaction request on to a Lightwalletd and returns the response.
    ///
    /// TODO: Process request here / call service function directly instead of forwarding request on to gRPC server, as this will queue request twice.
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
