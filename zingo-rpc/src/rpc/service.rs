//! Lightwallet service RPC implementations.

use hex::FromHex;
use zcash_client_backend::proto::{
    compact_formats::{CompactBlock, CompactTx},
    service::{
        compact_tx_streamer_server::CompactTxStreamer, Address, AddressList, Balance, BlockId,
        BlockRange, ChainSpec, Empty, Exclude, GetAddressUtxosArg, GetAddressUtxosReply,
        GetAddressUtxosReplyList, GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction,
        SendResponse, SubtreeRoot, TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};
use zebra_chain::block::Height;

use crate::{
    define_grpc_passthrough,
    jsonrpc::{connector::JsonRpcConnector, primitives::ProxyConsensusBranchIdHex},
    primitives::ProxyClient,
    utils::get_build_info,
};

impl CompactTxStreamer for ProxyClient {
    // fn get_latest_block<'life0, 'async_trait>(
    //     &'life0 self,
    //     request: tonic::Request<zcash_client_backend::proto::service::ChainSpec>,
    // ) -> core::pin::Pin<
    //     Box<
    //         dyn core::future::Future<
    //                 Output = std::result::Result<
    //                     tonic::Response<zcash_client_backend::proto::service::BlockId>,
    //                     tonic::Status,
    //                 >,
    //             > + core::marker::Send
    //             + 'async_trait,
    //     >,
    // >
    // where
    //     'life0: 'async_trait,
    //     Self: 'async_trait,
    // {
    //     println!("@zingoproxyd[nym]: Received call of get_latest_block.");
    //     Box::pin(async {
    //         let zebrad_info = ::zingo_netutils::GrpcConnector::new(self.zebrad_uri.clone())
    //             .get_client()
    //             .await;
    //         todo!("get_latest_block not yet implemented")
    //     })
    // }
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

    define_grpc_passthrough!(
        fn send_transaction(
            &self,
            request: tonic::Request<RawTransaction>,
        ) -> SendResponse
    );

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

    fn get_lightd_info<'life0, 'async_trait>(
        &'life0 self,
        _request: tonic::Request<Empty>,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<
                    Output = std::result::Result<
                        tonic::Response<zcash_client_backend::proto::service::LightdInfo>,
                        tonic::Status,
                    >,
                > + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        println!("@zingoproxyd: Received call of get_lightd_info.");
        // TODO: Add user and password as fields of ProxyClient and use here.
        // TODO: Return Nym_Address in get_lightd_info response, for use buy wallets.
        Box::pin(async {
            let zebrad_client = JsonRpcConnector::new(
                self.zebrad_uri.clone(),
                Some("xxxxxx".to_string()),
                Some("xxxxxx".to_string()),
            )
            .await;

            let zebra_info = zebrad_client
                .get_info()
                .await
                .map_err(|e| e.to_grpc_status())?;
            let blockchain_info = zebrad_client
                .get_blockchain_info()
                .await
                .map_err(|e| e.to_grpc_status())?;

            let sapling_id_str = "76b809bb";
            let sapling_id = ProxyConsensusBranchIdHex(
                zebra_chain::parameters::ConsensusBranchId::from_hex(sapling_id_str).unwrap(),
            );
            let sapling_height = blockchain_info
                .upgrades
                .get(&sapling_id)
                .map_or(Height(1), |sapling_json| sapling_json.activation_height);

            let (git_commit, branch, build_date, build_user, version) = get_build_info();

            let lightd_info = LightdInfo {
                version,
                vendor: "ZingoLabs ZingoProxyD".to_string(),
                taddr_support: true,
                chain_name: blockchain_info.chain,
                sapling_activation_height: sapling_height.0 as u64,
                consensus_branch_id: blockchain_info.consensus.chain_tip.0.to_string(),
                block_height: blockchain_info.blocks.0 as u64,
                git_commit,
                branch,
                build_date,
                build_user,
                estimated_height: blockchain_info.estimated_height.0 as u64,
                zcashd_build: zebra_info.build,
                zcashd_subversion: zebra_info.subversion,
            };

            Ok(tonic::Response::new(lightd_info))
        })
    }
    // define_grpc_passthrough!(
    //     fn get_lightd_info(
    //         &self,
    //         request: tonic::Request<Empty>,
    //     ) -> LightdInfo
    // );

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
