//! Wrapper implementation of LibRustZCash's CompactTXStreamerClient that also holds feature-gated, nym-enabled implementations.
//!
//! NOTE: Currently only send_transaction has been implemented.

use http::Uri;
use http_body::Body;
use zcash_client_backend::proto::compact_formats::{CompactBlock, CompactTx};
use zcash_client_backend::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient, RawTransaction, SendResponse,
};
use zcash_client_backend::proto::service::{
    Address, AddressList, Balance, BlockId, BlockRange, ChainSpec, Duration, Empty, Exclude,
    GetAddressUtxosArg, GetAddressUtxosReply, GetAddressUtxosReplyList, GetSubtreeRootsArg,
    LightdInfo, PingResponse, SubtreeRoot, TransparentAddressBlockFilter, TreeState, TxFilter,
};

use bytes::Bytes;
use std::error::Error as StdError;
use tonic::{self, codec::CompressionEncoding, Status};
use tonic::{service::interceptor::InterceptedService, transport::Endpoint};

use crate::{
    nym::utils::{deserialize_response, serialize_request},
    primitives::NymClient,
};

/// Wrapper struct for the Nym enabled CompactTxStreamerClient.
#[derive(Debug, Clone)]
pub struct NymTxStreamerClient<T> {
    compact_tx_streamer_client: CompactTxStreamerClient<T>,
}

impl NymTxStreamerClient<tonic::transport::Channel> {
    /// Attempt to create a new client by connecting to a given endpoint.
    pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<Box<dyn StdError>> + Send + Sync + 'static,
        <D as TryInto<Endpoint>>::Error: std::error::Error,
    {
        let client = CompactTxStreamerClient::connect(dst).await?;
        Ok(Self {
            compact_tx_streamer_client: client,
        })
    }
}

impl<T> NymTxStreamerClient<T>
where
    T: tonic::client::GrpcService<tonic::body::BoxBody> + Send + 'static,
    T::Error:
        std::error::Error + Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send + Sync,
    T::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error:
        std::error::Error + Into<Box<dyn std::error::Error + Send + Sync + 'static>> + Send + Sync,
{
    /// Creates a new gRPC clientwith the provided [`GrpcService`].
    pub fn new(inner: T) -> Self {
        Self {
            compact_tx_streamer_client: CompactTxStreamerClient::new(inner),
        }
    }

    /// Creates a new gRPC client with the provided [`GrpcService`] and `Uri`.
    ///
    /// The provided Uri will use only the scheme and authority parts as the path_and_query portion will be set for each method.
    pub fn with_origin(inner: T, origin: Uri) -> Self {
        Self {
            compact_tx_streamer_client: CompactTxStreamerClient::with_origin(inner, origin),
        }
    }

    /// Creates a new service with interceptor middleware.
    pub fn with_interceptor<F>(
        inner: T,
        interceptor: F,
    ) -> CompactTxStreamerClient<InterceptedService<T, F>>
    where
        F: tonic::service::Interceptor,
        T::ResponseBody: Default,
        T: tonic::codegen::Service<
            http::Request<tonic::body::BoxBody>,
            Response = http::Response<
                <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
            >,
        >,
        <T as tonic::codegen::Service<http::Request<tonic::body::BoxBody>>>::Error:
            Into<Box<dyn StdError>> + Send + Sync + 'static,
        <T as tonic::codegen::Service<
            http::Request<http_body::combinators::UnsyncBoxBody<bytes::Bytes, Status>>,
        >>::Error: std::error::Error,
    {
        CompactTxStreamerClient::new(InterceptedService::new(inner, interceptor))
    }

    /// Compress requests with the given encoding.
    ///
    /// This requires the server to support it otherwise it might respond with an error.
    #[must_use]
    pub fn send_compressed(self, encoding: CompressionEncoding) -> Self {
        Self {
            compact_tx_streamer_client: CompactTxStreamerClient::send_compressed(
                self.compact_tx_streamer_client,
                encoding,
            ),
        }
    }

    /// Enable decompressing responses.
    #[must_use]
    pub fn accept_compressed(self, encoding: CompressionEncoding) -> Self {
        Self {
            compact_tx_streamer_client: CompactTxStreamerClient::accept_compressed(
                self.compact_tx_streamer_client,
                encoding,
            ),
        }
    }

    /// Limits the maximum size of a decoded message.
    ///
    /// Default: `4MB`
    #[must_use]
    pub fn max_decoding_message_size(self, limit: usize) -> Self {
        Self {
            compact_tx_streamer_client: CompactTxStreamerClient::max_decoding_message_size(
                self.compact_tx_streamer_client,
                limit,
            ),
        }
    }

    /// Limits the maximum size of an encoded message.
    ///
    /// Default: `usize::MAX`
    #[must_use]
    pub fn max_encoding_message_size(self, limit: usize) -> Self {
        Self {
            compact_tx_streamer_client: CompactTxStreamerClient::max_encoding_message_size(
                self.compact_tx_streamer_client,
                limit,
            ),
        }
    }

    /// Return the height of the tip of the best chain.
    pub async fn get_latest_block(
        &mut self,
        request: impl tonic::IntoRequest<ChainSpec>,
    ) -> std::result::Result<tonic::Response<BlockId>, tonic::Status> {
        CompactTxStreamerClient::get_latest_block(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Return the compact block corresponding to the given block identifier.
    pub async fn get_block(
        &mut self,
        request: impl tonic::IntoRequest<BlockId>,
    ) -> std::result::Result<tonic::Response<CompactBlock>, tonic::Status> {
        CompactTxStreamerClient::get_block(&mut self.compact_tx_streamer_client, request).await
    }

    /// Same as GetBlock except actions contain only nullifiers.
    pub async fn get_block_nullifiers(
        &mut self,
        request: impl tonic::IntoRequest<BlockId>,
    ) -> std::result::Result<tonic::Response<CompactBlock>, tonic::Status> {
        CompactTxStreamerClient::get_block_nullifiers(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Return a list of consecutive compact blocks
    pub async fn get_block_range(
        &mut self,
        request: impl tonic::IntoRequest<BlockRange>,
    ) -> std::result::Result<tonic::Response<tonic::codec::Streaming<CompactBlock>>, tonic::Status>
    {
        CompactTxStreamerClient::get_block_range(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Same as GetBlockRange except actions contain only nullifiers.
    pub async fn get_block_range_nullifiers(
        &mut self,
        request: impl tonic::IntoRequest<BlockRange>,
    ) -> std::result::Result<tonic::Response<tonic::codec::Streaming<CompactBlock>>, tonic::Status>
    {
        CompactTxStreamerClient::get_block_range_nullifiers(
            &mut self.compact_tx_streamer_client,
            request,
        )
        .await
    }

    /// Return the requested full (not compact) transaction (as from zcashd).
    pub async fn get_transaction(
        &mut self,
        request: impl tonic::IntoRequest<TxFilter>,
    ) -> std::result::Result<tonic::Response<RawTransaction>, tonic::Status> {
        CompactTxStreamerClient::get_transaction(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Submit the given transaction to the Zcash network.
    ///
    /// If nym_addr is provided, the transaction is encoded and sent over the Nym mixnet.
    pub async fn send_transaction(
        &mut self,
        request: impl tonic::IntoRequest<RawTransaction>,
        nym_addr: Option<&str>,
    ) -> std::result::Result<tonic::Response<SendResponse>, Status> {
        match nym_addr {
            Some(addr) => {
                match nym_sphinx_addressing::clients::Recipient::try_from_base58_string(addr) {
                    Ok(_recipient) => {
                        let serialized_request =
                            match serialize_request(&request.into_request().into_inner()).await {
                                Ok(data) => data,
                                Err(e) => {
                                    return Err(Status::internal(format!(
                                        "Failed to serialize request: {}",
                                        e
                                    )))
                                }
                            };
                        let nym_conf_path = "/tmp/nym_client";
                        let mut client = NymClient::nym_spawn(nym_conf_path).await;
                        let response_data =
                            client.nym_forward(addr, serialized_request).await.unwrap();
                        client.nym_close().await;
                        let response: SendResponse =
                            match deserialize_response(response_data.as_slice()).await {
                                Ok(res) => res,
                                Err(e) => {
                                    return Err(Status::internal(format!(
                                        "Failed to decode response: {}",
                                        e
                                    )))
                                }
                            };
                        Ok(tonic::Response::new(response))
                    }
                    Err(e) => {
                        return Err(Status::invalid_argument(format!(
                            "Failed to parse nym address: {}",
                            e
                        )));
                    }
                }
            }
            None => {
                CompactTxStreamerClient::send_transaction(
                    &mut self.compact_tx_streamer_client,
                    request,
                )
                .await
            }
        }
    }

    /// Return the txids corresponding to the given t-address within the given block range.
    pub async fn get_taddress_txids(
        &mut self,
        request: impl tonic::IntoRequest<TransparentAddressBlockFilter>,
    ) -> std::result::Result<tonic::Response<tonic::codec::Streaming<RawTransaction>>, tonic::Status>
    {
        CompactTxStreamerClient::get_taddress_txids(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Return the balance corresponding to the given t-address.
    pub async fn get_taddress_balance(
        &mut self,
        request: impl tonic::IntoRequest<AddressList>,
    ) -> std::result::Result<tonic::Response<Balance>, tonic::Status> {
        CompactTxStreamerClient::get_taddress_balance(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Return the balance corresponding to the given t-address.
    ///
    /// TODO: Doc comment is ambiguous, add correct information.
    pub async fn get_taddress_balance_stream(
        &mut self,
        request: impl tonic::IntoStreamingRequest<Message = Address>,
    ) -> std::result::Result<tonic::Response<Balance>, tonic::Status> {
        CompactTxStreamerClient::get_taddress_balance_stream(
            &mut self.compact_tx_streamer_client,
            request,
        )
        .await
    }

    /// Return the compact transactions currently in the mempool; the results
    /// can be a few seconds out of date. If the Exclude list is empty, return
    /// all transactions; otherwise return all *except* those in the Exclude list
    /// (if any); this allows the client to avoid receiving transactions that it
    /// already has (from an earlier call to this rpc). The transaction IDs in the
    /// Exclude list can be shortened to any number of bytes to make the request
    /// more bandwidth-efficient; if two or more transactions in the mempool
    /// match a shortened txid, they are all sent (none is excluded). Transactions
    /// in the exclude list that don't exist in the mempool are ignored.
    pub async fn get_mempool_tx(
        &mut self,
        request: impl tonic::IntoRequest<Exclude>,
    ) -> std::result::Result<tonic::Response<tonic::codec::Streaming<CompactTx>>, tonic::Status>
    {
        CompactTxStreamerClient::get_mempool_tx(&mut self.compact_tx_streamer_client, request).await
    }

    /// Return a stream of current Mempool transactions. This will keep the output stream open while
    /// there are mempool transactions. It will close the returned stream when a new block is mined.
    pub async fn get_mempool_stream(
        &mut self,
        request: impl tonic::IntoRequest<Empty>,
    ) -> std::result::Result<tonic::Response<tonic::codec::Streaming<RawTransaction>>, tonic::Status>
    {
        CompactTxStreamerClient::get_mempool_stream(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// GetTreeState returns the note commitment tree state corresponding to the given block.
    /// See section 3.7 of the Zcash protocol specification. It returns several other useful
    /// values also (even though they can be obtained using GetBlock).
    /// The block can be specified by either height or hash.
    pub async fn get_tree_state(
        &mut self,
        request: impl tonic::IntoRequest<BlockId>,
    ) -> std::result::Result<tonic::Response<TreeState>, tonic::Status> {
        CompactTxStreamerClient::get_tree_state(&mut self.compact_tx_streamer_client, request).await
    }

    /// Returns the note commitment tree state.
    ///
    /// TODO: Doc comment is ambiguous, add correct information.
    pub async fn get_latest_tree_state(
        &mut self,
        request: impl tonic::IntoRequest<Empty>,
    ) -> std::result::Result<tonic::Response<TreeState>, tonic::Status> {
        CompactTxStreamerClient::get_latest_tree_state(
            &mut self.compact_tx_streamer_client,
            request,
        )
        .await
    }

    /// Returns a stream of information about roots of subtrees of the Sapling and Orchard
    /// note commitment trees.
    pub async fn get_subtree_roots(
        &mut self,
        request: impl tonic::IntoRequest<GetSubtreeRootsArg>,
    ) -> std::result::Result<tonic::Response<tonic::codec::Streaming<SubtreeRoot>>, tonic::Status>
    {
        CompactTxStreamerClient::get_subtree_roots(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Returns utxos belonging to the given address.
    pub async fn get_address_utxos(
        &mut self,
        request: impl tonic::IntoRequest<GetAddressUtxosArg>,
    ) -> std::result::Result<tonic::Response<GetAddressUtxosReplyList>, tonic::Status> {
        CompactTxStreamerClient::get_address_utxos(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Returns stream of utxos belonging to the given address.
    pub async fn get_address_utxos_stream(
        &mut self,
        request: impl tonic::IntoRequest<GetAddressUtxosArg>,
    ) -> std::result::Result<
        tonic::Response<tonic::codec::Streaming<GetAddressUtxosReply>>,
        tonic::Status,
    > {
        CompactTxStreamerClient::get_address_utxos_stream(
            &mut self.compact_tx_streamer_client,
            request,
        )
        .await
    }

    /// Return information about this lightwalletd instance and the blockchain.
    pub async fn get_lightd_info(
        &mut self,
        request: impl tonic::IntoRequest<Empty>,
    ) -> std::result::Result<tonic::Response<LightdInfo>, tonic::Status> {
        CompactTxStreamerClient::get_lightd_info(&mut self.compact_tx_streamer_client, request)
            .await
    }

    /// Testing-only, requires lightwalletd --ping-very-insecure (do not enable in production).
    pub async fn ping(
        &mut self,
        request: impl tonic::IntoRequest<Duration>,
    ) -> std::result::Result<tonic::Response<PingResponse>, tonic::Status> {
        CompactTxStreamerClient::ping(&mut self.compact_tx_streamer_client, request).await
    }
}
