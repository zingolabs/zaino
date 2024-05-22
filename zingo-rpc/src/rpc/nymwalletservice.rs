//! Client-side service RPC nym wrapper implementations.
//!
//! NOTE: DEPRICATED.

use std::env;
use tonic::{async_trait, Request, Response, Status};
use zcash_client_backend::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{
        compact_tx_streamer_server::CompactTxStreamer, Address, AddressList, Balance, BlockId,
        BlockRange, ChainSpec, Empty, Exclude, GetAddressUtxosArg, GetAddressUtxosReply,
        GetAddressUtxosReplyList, GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction,
        SendResponse, SubtreeRoot, TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};

use crate::{
    define_grpc_passthrough,
    nym::utils::{deserialize_response, serialize_request},
    primitives::{NymClient, ProxyClient},
};

#[async_trait]
impl CompactTxStreamer for ProxyClient {
    define_grpc_passthrough!(
        fn get_latest_block(
            &self,
            request: tonic::Request<ChainSpec>,
        ) -> BlockId
    );

    define_grpc_passthrough!(
        fn get_block(
            &self,
            request: tonic::Request<BlockId>,
        ) -> CompactBlock
    );

    #[doc = "Server streaming response type for the GetBlockRange method."]
    type GetBlockRangeStream = tonic::Streaming<CompactBlock>;

    define_grpc_passthrough!(
        fn get_block_range(
            &self,
            request: tonic::Request<BlockRange>,
        ) -> Self::GetBlockRangeStream
    );

    define_grpc_passthrough!(
        fn get_transaction(
            &self,
            request: tonic::Request<TxFilter>,
        ) -> RawTransaction
    );

    async fn send_transaction(
        &self,
        request: Request<RawTransaction>,
    ) -> Result<Response<SendResponse>, Status> {
        println!("@zingoproxyd[nym]: Received call of send_transaction.");

        //serialize RawTransaction
        let serialized_request = match serialize_request(&request.into_inner()).await {
            Ok(data) => data,
            Err(e) => {
                return Err(Status::internal(format!(
                    "Failed to serialize request: {}",
                    e
                )))
            }
        };

        //print request for testing:
        println!(
            "@zingoproxyd[nym][TEST]: Request sent: {:?}.",
            serialized_request
        );
        println!(
            "@zingoproxyd[nym][TEST]: Request length: {}.",
            serialized_request.len()
        );

        // -- forward request over nym
        let args: Vec<String> = env::args().collect();
        let recipient_address: String = args[1].clone();
        let nym_conf_path = "/tmp/nym_client";
        let mut client = NymClient::nym_spawn(nym_conf_path).await;
        let response_data = client
            .nym_forward(recipient_address.as_str(), serialized_request)
            .await
            .unwrap();
        client.nym_close().await;

        //print response for testing
        println!(
            "@zingoproxyd[nym][TEST]: Response received: {:?}.",
            response_data
        );
        println!(
            "@zingoproxyd[nym][TEST]: Response length: {}.",
            response_data.len()
        );

        //deserialize SendResponse
        let response: SendResponse = match deserialize_response(response_data.as_slice()).await {
            Ok(res) => res,
            Err(e) => {
                return Err(Status::internal(format!(
                    "Failed to decode response: {}",
                    e
                )))
            }
        };

        Ok(Response::new(response))
    }

    #[doc = "Server streaming response type for the GetTaddressTxids method."]
    type GetTaddressTxidsStream = tonic::Streaming<RawTransaction>;

    define_grpc_passthrough!(
        fn get_taddress_txids(
            &self,
            request: tonic::Request<TransparentAddressBlockFilter>,
        ) -> Self::GetTaddressTxidsStream
    );

    define_grpc_passthrough!(
        fn get_taddress_balance(
            &self,
            request: tonic::Request<AddressList>,
        ) -> Balance
    );

    /// This isn't easily definable with the macro, and I beleive it to be unused
    #[must_use]
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn get_taddress_balance_stream<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<tonic::Streaming<Address>>,
    ) -> ::core::pin::Pin<
        Box<
            dyn ::core::future::Future<Output = Result<tonic::Response<Balance>, tonic::Status>>
                + ::core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        todo!("this isn't expected to be called. Please implement this if you need it")
    }

    #[doc = "Server streaming response type for the GetMempoolTx method."]
    type GetMempoolTxStream = tonic::Streaming<CompactTx>;

    define_grpc_passthrough!(
        fn get_mempool_tx(
            &self,
            request: tonic::Request<Exclude>,
        ) -> Self::GetMempoolTxStream
    );

    #[doc = "Server streaming response type for the GetMempoolStream method."]
    type GetMempoolStreamStream = tonic::Streaming<RawTransaction>;

    define_grpc_passthrough!(
        fn get_mempool_stream(
            &self,
            request: tonic::Request<Empty>,
        ) -> Self::GetMempoolStreamStream
    );

    define_grpc_passthrough!(
        fn get_tree_state(
            &self,
            request: tonic::Request<BlockId>,
        ) -> TreeState
    );

    define_grpc_passthrough!(
        fn get_address_utxos(
            &self,
            request: tonic::Request<GetAddressUtxosArg>,
        ) -> GetAddressUtxosReplyList
    );

    #[doc = "Server streaming response type for the GetAddressUtxosStream method."]
    type GetAddressUtxosStreamStream = tonic::Streaming<GetAddressUtxosReply>;

    define_grpc_passthrough!(
        fn get_address_utxos_stream(
            &self,
            request: tonic::Request<GetAddressUtxosArg>,
        ) -> tonic::Streaming<GetAddressUtxosReply>
    );

    define_grpc_passthrough!(
        fn get_lightd_info(
            &self,
            request: tonic::Request<Empty>,
        ) -> LightdInfo
    );

    define_grpc_passthrough!(
        fn ping(
            &self,
            request: tonic::Request<zcash_client_backend::proto::service::Duration>,
        ) -> PingResponse
    );

    define_grpc_passthrough!(
        fn get_block_nullifiers(
            &self,
            request: tonic::Request<BlockId>,
        ) -> CompactBlock
    );

    define_grpc_passthrough!(
        fn get_block_range_nullifiers(
            &self,
            request: tonic::Request<BlockRange>,
        ) -> Self::GetBlockRangeNullifiersStream
    );
    #[doc = " Server streaming response type for the GetBlockRangeNullifiers method."]
    type GetBlockRangeNullifiersStream = tonic::Streaming<CompactBlock>;

    define_grpc_passthrough!(
        fn get_latest_tree_state(
            &self,
            request: tonic::Request<Empty>,
        ) -> TreeState
    );

    define_grpc_passthrough!(
        fn get_subtree_roots(
            &self,
            request: tonic::Request<GetSubtreeRootsArg>,
        ) -> Self::GetSubtreeRootsStream
    );

    #[doc = " Server streaming response type for the GetSubtreeRoots method."]
    type GetSubtreeRootsStream = tonic::Streaming<SubtreeRoot>;
}
