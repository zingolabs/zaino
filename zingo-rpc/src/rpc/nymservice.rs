//! Lightwallet service RPC Nym implementations.

use prost::Message;

use crate::{queue::request::ZingoProxyRequest, rpc::GrpcClient};

#[cfg(not(feature = "nym_poc"))]
use crate::proto::service::compact_tx_streamer_server::CompactTxStreamer;

#[cfg(feature = "nym_poc")]
use zcash_client_backend::proto::service::compact_tx_streamer_server::CompactTxStreamer;

impl GrpcClient {
    /// Processes gRPC requests coming from the nym server.
    pub async fn process_nym_request(
        &self,
        request: &ZingoProxyRequest,
    ) -> Result<Vec<u8>, tonic::Status> {
        match request {
            ZingoProxyRequest::NymServerRequest(request) => match request.get_request().method().as_str() {
                "GetLightdInfo" => match prost::Message::decode(&request.get_request().body()[..]) {
                    Ok(input) => {
                        let tonic_request = tonic::Request::new(input);
                        let tonic_response = self.get_lightd_info(tonic_request)
                            .await?.into_inner();

                        let mut response_vec = Vec::new();
                        tonic_response.encode(&mut response_vec).map_err(|e| {
                            tonic::Status::internal(format!(
                                "Failed to encode response: {}",
                                e
                            ))
                        })?;
                        Ok(response_vec)
                    }
                    Err(e) => Err(tonic::Status::internal(format!(
                        "Failed to decode request: {}",
                        e
                    ))),
                },
                "SendTransaction" => match prost::Message::decode(&request.get_request().body()[..]) {
                    Ok(input) => {
                        let tonic_request = tonic::Request::new(input);
                        let tonic_response = self.send_transaction(tonic_request)
                            .await?.into_inner();
                        let mut response_vec = Vec::new();
                        tonic_response.encode(&mut response_vec).map_err(|e| {
                            tonic::Status::internal(format!(
                                "Failed to encode response: {}",
                                e
                            ))
                        })?;
                        Ok(response_vec)
                    }
                    Err(e) => Err(tonic::Status::internal(format!(
                        "Failed to decode request: {}",
                        e
                    ))),
                },
                "get_latest_block" |
                "get_block" |
                "get_block_nullifiers" |
                "get_block_range" |
                "get_block_range_nullifiers" |
                "get_transaction" |
                "send_transaction" |
                "get_taddress_txids" |
                "get_taddress_balance" |
                "get_taddress_balance_stream" |
                "get_mempool_tx" |
                "get_mempool_stream" |
                "get_tree_state" |
                "get_latest_tree_state" |
                "get_subtree_roots" |
                "get_address_utxos" |
                "get_address_utxos_stream" |
                "ping" => {
                    Err(tonic::Status::unimplemented("RPC not yet implemented over nym. If you require this RPC please open an issue or PR at the Zingo-Proxy github (https://github.com/zingolabs/zingo-proxy)."))
                    },
                _ => Err(tonic::Status::invalid_argument("Incorrect Method String")),
            },
            _ => Err(tonic::Status::invalid_argument("Incorrect Request Type")),
        }
    }
}
