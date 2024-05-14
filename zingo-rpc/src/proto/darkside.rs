//! Generated from darkside.proto.
//!
//! Remember that proto3 fields are all optional. A field that is not present will be set to its zero value.
//! bytes fields of hashes are in canonical little-endian format.

use zcash_client_backend::proto::service::{
    BlockId, Empty, GetAddressUtxosReply, RawTransaction, ShieldedProtocol, SubtreeRoot, TreeState,
};

#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideMetaState {
    #[prost(int32, tag = "1")]
    pub sapling_activation: i32,
    #[prost(string, tag = "2")]
    pub branch_id: ::prost::alloc::string::String,
    #[prost(string, tag = "3")]
    pub chain_name: ::prost::alloc::string::String,
    #[prost(uint32, tag = "4")]
    pub start_sapling_commitment_tree_size: u32,
    #[prost(uint32, tag = "5")]
    pub start_orchard_commitment_tree_size: u32,
}

/// A block is a hex-encoded string.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideBlock {
    #[prost(string, tag = "1")]
    pub block: ::prost::alloc::string::String,
}

/// DarksideBlocksURL is typically something like:
/// <https://raw.githubusercontent.com/zcash-hackworks/darksidewalletd-test-data/master/basic-reorg/before-reorg.txt>
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideBlocksUrl {
    #[prost(string, tag = "1")]
    pub url: ::prost::alloc::string::String,
}

/// DarksideTransactionsURL refers to an HTTP source that contains a list
/// of hex-encoded transactions, one per line, that are to be associated
/// with the given height (fake-mined into the block at that height)
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideTransactionsUrl {
    #[prost(int32, tag = "1")]
    pub height: i32,
    #[prost(string, tag = "2")]
    pub url: ::prost::alloc::string::String,
}

#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideHeight {
    #[prost(int32, tag = "1")]
    pub height: i32,
}

#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideEmptyBlocks {
    #[prost(int32, tag = "1")]
    pub height: i32,
    #[prost(int32, tag = "2")]
    pub nonce: i32,
    #[prost(int32, tag = "3")]
    pub count: i32,
}

#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DarksideSubtreeRoots {
    #[prost(enumeration = "ShieldedProtocol", tag = "1")]
    pub shielded_protocol: i32,
    #[prost(uint32, tag = "2")]
    pub start_index: u32,
    #[prost(message, repeated, tag = "3")]
    pub subtree_roots: ::prost::alloc::vec::Vec<SubtreeRoot>,
}

/// Generated client implementations.
pub mod darkside_streamer_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use super::*;
    use tonic::codegen::http::Uri;
    use tonic::codegen::*;

    /// Darksidewalletd maintains two staging areas, blocks and transactions. The
    /// Stage*() gRPCs add items to the staging area; ApplyStaged() "applies" everything
    /// in the staging area to the working (operational) state that the mock zcashd
    /// serves; transactions are placed into their corresponding blocks (by height).
    #[derive(Debug, Clone)]
    pub struct DarksideStreamerClient<T> {
        inner: tonic::client::Grpc<T>,
    }

    impl DarksideStreamerClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }

    impl<T> DarksideStreamerClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }

        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }

        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> DarksideStreamerClient<InterceptedService<T, F>>
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
                Into<StdError> + Send + Sync,
        {
            DarksideStreamerClient::new(InterceptedService::new(inner, interceptor))
        }

        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }

        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }

        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }

        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }

        /// Reset reverts all darksidewalletd state (active block range, latest height,
        /// staged blocks and transactions) and lightwalletd state (cache) to empty,
        /// the same as the initial state. This occurs synchronously and instantaneously;
        /// no reorg happens in lightwalletd. This is good to do before each independent
        /// test so that no state leaks from one test to another.
        /// Also sets (some of) the values returned by GetLightdInfo(). The Sapling
        /// activation height specified here must be where the block range starts.
        pub async fn reset(
            &mut self,
            request: impl tonic::IntoRequest<DarksideMetaState>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/Reset",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "Reset",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// StageBlocksStream accepts a list of blocks and saves them into the blocks
        /// staging area until ApplyStaged() is called; there is no immediate effect on
        /// the mock zcashd. Blocks are hex-encoded. Order is important, see ApplyStaged.
        pub async fn stage_blocks_stream(
            &mut self,
            request: impl tonic::IntoStreamingRequest<Message = DarksideBlock>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageBlocksStream",
            );
            let mut req = request.into_streaming_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "StageBlocksStream",
            ));
            self.inner.client_streaming(req, path, codec).await
        }

        /// StageBlocks is the same as StageBlocksStream() except the blocks are fetched
        /// from the given URL. Blocks are one per line, hex-encoded (not JSON).
        pub async fn stage_blocks(
            &mut self,
            request: impl tonic::IntoRequest<DarksideBlocksUrl>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageBlocks",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "StageBlocks",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// StageBlocksCreate is like the previous two, except it creates 'count'
        /// empty blocks at consecutive heights starting at height 'height'. The
        /// 'nonce' is part of the header, so it contributes to the block hash; this
        /// lets you create identical blocks (same transactions and height), but with
        /// different hashes.
        pub async fn stage_blocks_create(
            &mut self,
            request: impl tonic::IntoRequest<DarksideEmptyBlocks>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageBlocksCreate",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "StageBlocksCreate",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// StageTransactionsStream stores the given transaction-height pairs in the
        /// staging area until ApplyStaged() is called. Note that these transactions
        /// are not returned by the production GetTransaction() gRPC until they
        /// appear in a "mined" block (contained in the active blockchain presented
        /// by the mock zcashd).
        pub async fn stage_transactions_stream(
            &mut self,
            request: impl tonic::IntoStreamingRequest<Message = RawTransaction>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageTransactionsStream",
            );
            let mut req = request.into_streaming_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "StageTransactionsStream",
            ));
            self.inner.client_streaming(req, path, codec).await
        }

        /// StageTransactions is the same except the transactions are fetched from
        /// the given url. They are all staged into the block at the given height.
        /// Staging transactions to different heights requires multiple calls.
        pub async fn stage_transactions(
            &mut self,
            request: impl tonic::IntoRequest<DarksideTransactionsUrl>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageTransactions",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "StageTransactions",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// ApplyStaged iterates the list of blocks that were staged by the
        /// StageBlocks*() gRPCs, in the order they were staged, and "merges" each
        /// into the active, working blocks list that the mock zcashd is presenting
        /// to lightwalletd. Even as each block is applied, the active list can't
        /// have gaps; if the active block range is 1000-1006, and the staged block
        /// range is 1003-1004, the resulting range is 1000-1004, with 1000-1002
        /// unchanged, blocks 1003-1004 from the new range, and 1005-1006 dropped.
        ///
        /// After merging all blocks, ApplyStaged() appends staged transactions (in
        /// the order received) into each one's corresponding (by height) block
        /// The staging area is then cleared.
        ///
        /// The argument specifies the latest block height that mock zcashd reports
        /// (i.e. what's returned by GetLatestBlock). Note that ApplyStaged() can
        /// also be used to simply advance the latest block height presented by mock
        /// zcashd. That is, there doesn't need to be anything in the staging area.
        pub async fn apply_staged(
            &mut self,
            request: impl tonic::IntoRequest<DarksideHeight>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/ApplyStaged",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "ApplyStaged",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// Calls to the production gRPC SendTransaction() store the transaction in
        /// a separate area (not the staging area); this method returns all transactions
        /// in this separate area, which is then cleared. The height returned
        /// with each transaction is -1 (invalid) since these transactions haven't
        /// been mined yet. The intention is that the transactions returned here can
        /// then, for example, be given to StageTransactions() to get them "mined"
        /// into a specified block on the next ApplyStaged().
        pub async fn get_incoming_transactions(
            &mut self,
            request: impl tonic::IntoRequest<Empty>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<RawTransaction>>,
            tonic::Status,
        > {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/GetIncomingTransactions",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "GetIncomingTransactions",
            ));
            self.inner.server_streaming(req, path, codec).await
        }

        /// Clear the incoming transaction pool.
        pub async fn clear_incoming_transactions(
            &mut self,
            request: impl tonic::IntoRequest<Empty>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/ClearIncomingTransactions",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "ClearIncomingTransactions",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// Add a GetAddressUtxosReply entry to be returned by GetAddressUtxos().
        /// There is no staging or applying for these, very simple.
        pub async fn add_address_utxo(
            &mut self,
            request: impl tonic::IntoRequest<GetAddressUtxosReply>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/AddAddressUtxo",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "AddAddressUtxo",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// Clear the list of GetAddressUtxos entries (can't fail)
        pub async fn clear_address_utxo(
            &mut self,
            request: impl tonic::IntoRequest<Empty>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/ClearAddressUtxo",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "ClearAddressUtxo",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// Adds a GetTreeState to the tree state cache
        pub async fn add_tree_state(
            &mut self,
            request: impl tonic::IntoRequest<TreeState>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/AddTreeState",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "AddTreeState",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// Removes a GetTreeState for the given height from cache if present (can't fail)
        pub async fn remove_tree_state(
            &mut self,
            request: impl tonic::IntoRequest<BlockId>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/RemoveTreeState",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "RemoveTreeState",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// Clear the list of GetTreeStates entries (can't fail)
        pub async fn clear_all_tree_states(
            &mut self,
            request: impl tonic::IntoRequest<Empty>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/ClearAllTreeStates",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "ClearAllTreeStates",
            ));
            self.inner.unary(req, path, codec).await
        }

        /// Sets the subtree roots cache (for GetSubtreeRoots),
        /// replacing any existing entries
        pub async fn set_subtree_roots(
            &mut self,
            request: impl tonic::IntoRequest<DarksideSubtreeRoots>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status> {
            self.inner.ready().await.map_err(|e| {
                tonic::Status::new(
                    tonic::Code::Unknown,
                    format!("Service was not ready: {}", e.into()),
                )
            })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/SetSubtreeRoots",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new(
                "cash.z.wallet.sdk.rpc.DarksideStreamer",
                "SetSubtreeRoots",
            ));
            self.inner.unary(req, path, codec).await
        }
    }
}

/// Generated server implementations.
pub mod darkside_streamer_server {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use super::*;
    use tonic::codegen::*;

    /// Generated trait containing gRPC methods that should be implemented for use with DarksideStreamerServer.
    #[async_trait]
    pub trait DarksideStreamer: Send + Sync + 'static {
        /// Reset reverts all darksidewalletd state (active block range, latest height,
        /// staged blocks and transactions) and lightwalletd state (cache) to empty,
        /// the same as the initial state. This occurs synchronously and instantaneously;
        /// no reorg happens in lightwalletd. This is good to do before each independent
        /// test so that no state leaks from one test to another.
        /// Also sets (some of) the values returned by GetLightdInfo(). The Sapling
        /// activation height specified here must be where the block range starts.
        async fn reset(
            &self,
            request: tonic::Request<DarksideMetaState>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// StageBlocksStream accepts a list of blocks and saves them into the blocks
        /// staging area until ApplyStaged() is called; there is no immediate effect on
        /// the mock zcashd. Blocks are hex-encoded. Order is important, see ApplyStaged.
        async fn stage_blocks_stream(
            &self,
            request: tonic::Request<tonic::Streaming<DarksideBlock>>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// StageBlocks is the same as StageBlocksStream() except the blocks are fetched
        /// from the given URL. Blocks are one per line, hex-encoded (not JSON).
        async fn stage_blocks(
            &self,
            request: tonic::Request<DarksideBlocksUrl>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// StageBlocksCreate is like the previous two, except it creates 'count'
        /// empty blocks at consecutive heights starting at height 'height'. The
        /// 'nonce' is part of the header, so it contributes to the block hash; this
        /// lets you create identical blocks (same transactions and height), but with
        /// different hashes.
        async fn stage_blocks_create(
            &self,
            request: tonic::Request<DarksideEmptyBlocks>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// StageTransactionsStream stores the given transaction-height pairs in the
        /// staging area until ApplyStaged() is called. Note that these transactions
        /// are not returned by the production GetTransaction() gRPC until they
        /// appear in a "mined" block (contained in the active blockchain presented
        /// by the mock zcashd).
        async fn stage_transactions_stream(
            &self,
            request: tonic::Request<tonic::Streaming<RawTransaction>>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// StageTransactions is the same except the transactions are fetched from
        /// the given url. They are all staged into the block at the given height.
        /// Staging transactions to different heights requires multiple calls.
        async fn stage_transactions(
            &self,
            request: tonic::Request<DarksideTransactionsUrl>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// ApplyStaged iterates the list of blocks that were staged by the
        /// StageBlocks*() gRPCs, in the order they were staged, and "merges" each
        /// into the active, working blocks list that the mock zcashd is presenting
        /// to lightwalletd. Even as each block is applied, the active list can't
        /// have gaps; if the active block range is 1000-1006, and the staged block
        /// range is 1003-1004, the resulting range is 1000-1004, with 1000-1002
        /// unchanged, blocks 1003-1004 from the new range, and 1005-1006 dropped.
        ///
        /// After merging all blocks, ApplyStaged() appends staged transactions (in
        /// the order received) into each one's corresponding (by height) block
        /// The staging area is then cleared.
        ///
        /// The argument specifies the latest block height that mock zcashd reports
        /// (i.e. what's returned by GetLatestBlock). Note that ApplyStaged() can
        /// also be used to simply advance the latest block height presented by mock
        /// zcashd. That is, there doesn't need to be anything in the staging area.
        async fn apply_staged(
            &self,
            request: tonic::Request<DarksideHeight>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// Server streaming response type for the GetIncomingTransactions method.
        type GetIncomingTransactionsStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<RawTransaction, tonic::Status>,
            > + Send
            + 'static;

        /// Calls to the production gRPC SendTransaction() store the transaction in
        /// a separate area (not the staging area); this method returns all transactions
        /// in this separate area, which is then cleared. The height returned
        /// with each transaction is -1 (invalid) since these transactions haven't
        /// been mined yet. The intention is that the transactions returned here can
        /// then, for example, be given to StageTransactions() to get them "mined"
        /// into a specified block on the next ApplyStaged().
        async fn get_incoming_transactions(
            &self,
            request: tonic::Request<Empty>,
        ) -> std::result::Result<tonic::Response<Self::GetIncomingTransactionsStream>, tonic::Status>;

        /// Clear the incoming transaction pool.
        async fn clear_incoming_transactions(
            &self,
            request: tonic::Request<Empty>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// Add a GetAddressUtxosReply entry to be returned by GetAddressUtxos().
        /// There is no staging or applying for these, very simple.
        async fn add_address_utxo(
            &self,
            request: tonic::Request<GetAddressUtxosReply>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// Clear the list of GetAddressUtxos entries (can't fail)
        async fn clear_address_utxo(
            &self,
            request: tonic::Request<Empty>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// Adds a GetTreeState to the tree state cache
        async fn add_tree_state(
            &self,
            request: tonic::Request<TreeState>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// Removes a GetTreeState for the given height from cache if present (can't fail)
        async fn remove_tree_state(
            &self,
            request: tonic::Request<BlockId>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// Clear the list of GetTreeStates entries (can't fail)
        async fn clear_all_tree_states(
            &self,
            request: tonic::Request<Empty>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;

        /// Sets the subtree roots cache (for GetSubtreeRoots),
        /// replacing any existing entries
        async fn set_subtree_roots(
            &self,
            request: tonic::Request<DarksideSubtreeRoots>,
        ) -> std::result::Result<tonic::Response<Empty>, tonic::Status>;
    }

    /// Darksidewalletd maintains two staging areas, blocks and transactions. The
    /// Stage*() gRPCs add items to the staging area; ApplyStaged() "applies" everything
    /// in the staging area to the working (operational) state that the mock zcashd
    /// serves; transactions are placed into their corresponding blocks (by height).
    #[derive(Debug)]
    pub struct DarksideStreamerServer<T: DarksideStreamer> {
        inner: _Inner<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }

    struct _Inner<T>(Arc<T>);

    impl<T: DarksideStreamer> DarksideStreamerServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }

        pub fn from_arc(inner: Arc<T>) -> Self {
            let inner = _Inner(inner);
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }

        pub fn with_interceptor<F>(inner: T, interceptor: F) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }

        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }

        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }

        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }

        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }

    impl<T, B> tonic::codegen::Service<http::Request<B>> for DarksideStreamerServer<T>
    where
        T: DarksideStreamer,
        B: Body + Send + 'static,
        B::Error: Into<StdError> + Send + 'static,
    {
        type Response = http::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            let inner = self.inner.clone();
            match req.uri().path() {
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/Reset" => {
                    #[allow(non_camel_case_types)]
                    struct ResetSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<DarksideMetaState> for ResetSvc<T> {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<DarksideMetaState>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::reset(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ResetSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageBlocksStream" => {
                    #[allow(non_camel_case_types)]
                    struct StageBlocksStreamSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::ClientStreamingService<DarksideBlock>
                        for StageBlocksStreamSvc<T>
                    {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<tonic::Streaming<DarksideBlock>>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::stage_blocks_stream(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = StageBlocksStreamSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.client_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageBlocks" => {
                    #[allow(non_camel_case_types)]
                    struct StageBlocksSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<DarksideBlocksUrl> for StageBlocksSvc<T> {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<DarksideBlocksUrl>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::stage_blocks(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = StageBlocksSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageBlocksCreate" => {
                    #[allow(non_camel_case_types)]
                    struct StageBlocksCreateSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<DarksideEmptyBlocks>
                        for StageBlocksCreateSvc<T>
                    {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<DarksideEmptyBlocks>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::stage_blocks_create(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = StageBlocksCreateSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageTransactionsStream" => {
                    #[allow(non_camel_case_types)]
                    struct StageTransactionsStreamSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::ClientStreamingService<RawTransaction>
                        for StageTransactionsStreamSvc<T>
                    {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<tonic::Streaming<RawTransaction>>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::stage_transactions_stream(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = StageTransactionsStreamSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.client_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/StageTransactions" => {
                    #[allow(non_camel_case_types)]
                    struct StageTransactionsSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<DarksideTransactionsUrl>
                        for StageTransactionsSvc<T>
                    {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<DarksideTransactionsUrl>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::stage_transactions(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = StageTransactionsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/ApplyStaged" => {
                    #[allow(non_camel_case_types)]
                    struct ApplyStagedSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<DarksideHeight> for ApplyStagedSvc<T> {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<DarksideHeight>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::apply_staged(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ApplyStagedSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/GetIncomingTransactions" => {
                    #[allow(non_camel_case_types)]
                    struct GetIncomingTransactionsSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::ServerStreamingService<Empty>
                        for GetIncomingTransactionsSvc<T>
                    {
                        type Response = RawTransaction;
                        type ResponseStream = T::GetIncomingTransactionsStream;
                        type Future =
                            BoxFuture<tonic::Response<Self::ResponseStream>, tonic::Status>;
                        fn call(&mut self, request: tonic::Request<Empty>) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::get_incoming_transactions(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = GetIncomingTransactionsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/ClearIncomingTransactions" => {
                    #[allow(non_camel_case_types)]
                    struct ClearIncomingTransactionsSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<Empty> for ClearIncomingTransactionsSvc<T> {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(&mut self, request: tonic::Request<Empty>) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::clear_incoming_transactions(
                                    &inner, request,
                                )
                                .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ClearIncomingTransactionsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/AddAddressUtxo" => {
                    #[allow(non_camel_case_types)]
                    struct AddAddressUtxoSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<GetAddressUtxosReply>
                        for AddAddressUtxoSvc<T>
                    {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<GetAddressUtxosReply>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::add_address_utxo(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = AddAddressUtxoSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/ClearAddressUtxo" => {
                    #[allow(non_camel_case_types)]
                    struct ClearAddressUtxoSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<Empty> for ClearAddressUtxoSvc<T> {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(&mut self, request: tonic::Request<Empty>) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::clear_address_utxo(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ClearAddressUtxoSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/AddTreeState" => {
                    #[allow(non_camel_case_types)]
                    struct AddTreeStateSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<TreeState> for AddTreeStateSvc<T> {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(&mut self, request: tonic::Request<TreeState>) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::add_tree_state(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = AddTreeStateSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/RemoveTreeState" => {
                    #[allow(non_camel_case_types)]
                    struct RemoveTreeStateSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<BlockId> for RemoveTreeStateSvc<T> {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(&mut self, request: tonic::Request<BlockId>) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::remove_tree_state(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = RemoveTreeStateSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/ClearAllTreeStates" => {
                    #[allow(non_camel_case_types)]
                    struct ClearAllTreeStatesSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<Empty> for ClearAllTreeStatesSvc<T> {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(&mut self, request: tonic::Request<Empty>) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::clear_all_tree_states(&inner, request)
                                    .await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = ClearAllTreeStatesSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/cash.z.wallet.sdk.rpc.DarksideStreamer/SetSubtreeRoots" => {
                    #[allow(non_camel_case_types)]
                    struct SetSubtreeRootsSvc<T: DarksideStreamer>(pub Arc<T>);
                    impl<T: DarksideStreamer> tonic::server::UnaryService<DarksideSubtreeRoots>
                        for SetSubtreeRootsSvc<T>
                    {
                        type Response = Empty;
                        type Future = BoxFuture<tonic::Response<Self::Response>, tonic::Status>;
                        fn call(
                            &mut self,
                            request: tonic::Request<DarksideSubtreeRoots>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as DarksideStreamer>::set_subtree_roots(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let inner = inner.0;
                        let method = SetSubtreeRootsSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => Box::pin(async move {
                    Ok(http::Response::builder()
                        .status(200)
                        .header("grpc-status", "12")
                        .header("content-type", "application/grpc")
                        .body(empty_body())
                        .unwrap())
                }),
            }
        }
    }

    impl<T: DarksideStreamer> Clone for DarksideStreamerServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }

    impl<T: DarksideStreamer> Clone for _Inner<T> {
        fn clone(&self) -> Self {
            Self(Arc::clone(&self.0))
        }
    }

    impl<T: std::fmt::Debug> std::fmt::Debug for _Inner<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }

    impl<T: DarksideStreamer> tonic::server::NamedService for DarksideStreamerServer<T> {
        const NAME: &'static str = "cash.z.wallet.sdk.rpc.DarksideStreamer";
    }
}
