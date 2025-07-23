//! Lightwallet service RPC implementations.

use futures::StreamExt;
use tracing::info;

use crate::rpc::GrpcClient;
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        compact_tx_streamer_server::CompactTxStreamer, Address, AddressList, Balance, BlockId,
        BlockRange, ChainSpec, Duration, Empty, Exclude, GetAddressUtxosArg,
        GetAddressUtxosReplyList, GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction,
        SendResponse, TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};
use zaino_state::{
    AddressStream, CompactBlockStream, CompactTransactionStream, LightWalletIndexer,
    RawTransactionStream, SubtreeRootReplyStream, UtxoReplyStream, ZcashIndexer,
};

/// A helper macro invoked by implement_client_methods, as the
/// internals differ slightly in the streaming return case
/// compared to the 'normal' case. This should never
/// be invoked directly.
macro_rules! client_method_helper {
    // all of these are hard-coded by implement_client_methods,
    // and need to be passed in due to macros not being able to
    // access variables in an outer scope unless they are passed in
    ($self:ident $input:ident $method_name:ident) => {
        tonic::Response::new(
            // offload to the service_subscriber's implementation of
            // $method_name, unpacking $input
            $self
                .service_subscriber
                .inner_ref()
                .$method_name($input.into_inner())
                .await
                .map_err(Into::into)?,
        )
    };
    // in the case of Streaming return types, we need an additional
    // invocation of Box::pin
    (streaming $self:ident $input:ident $method_name:ident) => {
        // extra Box::pin here
        tonic::Response::new(Box::pin(
            $self
                .service_subscriber
                .inner_ref()
                .$method_name($input.into_inner())
                .await
                .map_err(Into::into)?,
        ))
    };
    // for the no-input variant
    (empty $self:ident $input:ident $method_name:ident) => {
        tonic::Response::new(
            $self
                .service_subscriber
                .inner_ref()
                .$method_name()
                .await
                .map_err(Into::into)?,
        )
    };
    // WOMBO-COMBO!!
    (streamingempty $self:ident $input:ident $method_name:ident) => {
        // extra Box::pin here
        tonic::Response::new(Box::pin(
            $self
                .service_subscriber
                .inner_ref()
                .$method_name()
                .await
                .map_err(Into::into)?,
        ))
    };
}

/// A macro to remove the boilerplate of implementing 30-ish client
/// methods with the same logic, aside from the types, names, and
/// streaming/nonstreaming.
///
/// Arguments:
/// comment method_name(input_type) -> return \[as streaming\],
/// \[comment\] A str literal to be used a doc-comment for the method.
/// \[method_name\] The name of the method to implement
/// \[input_type\] The type of the tonic Request to accept as an argument
/// \[as streaming/empty\] the optional literal characters
///     'as streaming', 'as empty', or 'as streamingempty'
///     needed when the return type is a Streaming type, and/or
///     the argument type isn't used
/// \[return\] the return type of the function
macro_rules! implement_client_methods {
    ($($comment:literal $method_name:ident($input_type:ty ) -> $return:ty $( as $streaming:ident)? ,)+) => {
        $(
            #[doc = $comment]
            fn $method_name<'life, 'async_trait>(
                &'life self,
                __input: tonic::Request<$input_type>,
            ) -> core::pin::Pin<
                Box<
                    dyn core::future::Future<
                            Output = std::result::Result<tonic::Response<$return>, tonic::Status>,
                        > + core::marker::Send
                        + 'async_trait,
                >,
            >
            where
                'life: 'async_trait,
                Self: 'async_trait,
            {
                info!("[TEST] Received call of {}.", stringify!($method_name));
                Box::pin(async {
                    Ok(
                        // here we pass in pinbox, to optionally add
                        // Box::pin to the returned response type for
                        // streaming
                        client_method_helper!($($streaming)? self __input $method_name)
                    )
                })
            }
        )+
    };
}

impl<Indexer: ZcashIndexer + LightWalletIndexer> CompactTxStreamer for GrpcClient<Indexer>
where
    Indexer::Error: Into<tonic::Status>,
{
    implement_client_methods!(
        "Return the height of the tip of the best chain."
        get_latest_block(ChainSpec) -> BlockId as empty,
        "Return the compact block corresponding to the given block identifier."
        get_block(BlockId) -> CompactBlock,
        "Same as GetBlock except actions contain only nullifiers."
        get_block_nullifiers(BlockId) -> CompactBlock,
        "Return a list of consecutive compact blocks."
        get_block_range(BlockRange) -> Self::GetBlockRangeStream as streaming,
        "Same as GetBlockRange except actions contain only nullifiers."
        get_block_range_nullifiers(BlockRange) -> Self::GetBlockRangeStream as streaming,
        "Return the requested full (not compact) transaction (as from zcashd)."
        get_transaction(TxFilter) -> RawTransaction,
        "submit the given transaction to the zcash network."
        send_transaction(RawTransaction) -> SendResponse,
        "This name is misleading, returns the full transactions that have either inputs or outputs connected to the given transparent address."
        get_taddress_txids(TransparentAddressBlockFilter) -> Self::GetTaddressTxidsStream as streaming,
        "Returns the total balance for a list of taddrs"
        get_taddress_balance(AddressList) -> Balance,
        "Return the compact transactions currently in the mempool; the results \
        can be a few seconds out of date. If the Exclude list is empty, return \
        all transactions; otherwise return all *except* those in the Exclude list \
        (if any); this allows the client to avoid receiving transactions that it \
        already has (from an earlier call to this rpc). The transaction IDs in the \
        Exclude list can be shortened to any number of bytes to make the request \
        more bandwidth-efficient; if two or more transactions in the mempool \
        match a shortened txid, they are all sent (none is excluded). Transactions \
        in the exclude list that don't exist in the mempool are ignored."
        get_mempool_tx(Exclude) -> Self::GetMempoolTxStream as streaming,
        "GetTreeState returns the note commitment tree state corresponding to the given block. \
        See section 3.7 of the Zcash protocol specification. It returns several other useful \
        values also (even though they can be obtained using GetBlock).
        The block can be specified by either height or hash."
        get_tree_state(BlockId) -> TreeState,
        "Returns a stream of information about roots of subtrees of the Sapling and Orchard \
        note commitment trees."
        get_subtree_roots(GetSubtreeRootsArg) -> Self::GetSubtreeRootsStream as streaming,
        "Returns all unspent outputs for a list of addresses. \
         \
        Ignores all utxos below block height [GetAddressUtxosArg.start_height]. \
        Returns max [GetAddressUtxosArg.max_entries] utxos, or \
        unrestricted if [GetAddressUtxosArg.max_entries] = 0. \
        Utxos are collected and returned as a single Vec."
        get_address_utxos(GetAddressUtxosArg) -> GetAddressUtxosReplyList,
        "Returns all unspent outputs for a list of addresses. \
        Ignores all utxos below block height [GetAddressUtxosArg. start_height]. \
        Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0. \
        Utxos are returned in a stream."
        get_address_utxos_stream(GetAddressUtxosArg) -> Self::GetAddressUtxosStreamStream as streaming,
        "Return information about this lightwalletd instance and the blockchain"
        get_lightd_info(Empty) -> LightdInfo as empty,
        "GetLatestTreeState returns the note commitment tree state corresponding to the chain tip."
        get_latest_tree_state(Empty) -> TreeState as empty,
        "Return a stream of current Mempool transactions. This will keep the output stream open while \
        there are mempool transactions. It will close the returned stream when a new block is mined."
        get_mempool_stream(Empty) -> Self::GetMempoolStreamStream as streamingempty,
        "Testing-only, requires lightwalletd --ping-very-insecure (do not enable in production) [from zebrad] \
        This RPC has not been implemented as it is not currently used by zingolib. \
        If you require this RPC please open an issue or PR at [https://github.com/zingolabs/zaino]\
        (https://github.com/zingolabs/zaino)."

        ping(Duration) -> PingResponse,
    );

    /// Server streaming response type for the GetBlockRange method.
    #[doc = "Server streaming response type for the GetBlockRange method."]
    type GetBlockRangeStream = std::pin::Pin<Box<CompactBlockStream>>;

    /// Server streaming response type for the GetBlockRangeNullifiers method.
    #[doc = " Server streaming response type for the GetBlockRangeNullifiers method."]
    type GetBlockRangeNullifiersStream = std::pin::Pin<Box<CompactBlockStream>>;

    /// Server streaming response type for the GetTaddressTxids method.
    #[doc = "Server streaming response type for the GetTaddressTxids method."]
    type GetTaddressTxidsStream = std::pin::Pin<Box<RawTransactionStream>>;

    /// Returns the total balance for a list of taddrs
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn get_taddress_balance_stream<'life0, 'async_trait>(
        &'life0 self,
        request: tonic::Request<tonic::Streaming<Address>>,
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
        info!("[TEST] Received call of get_taddress_balance_stream.");
        Box::pin(async {
            let (channel_tx, channel_rx) =
                tokio::sync::mpsc::channel::<Result<Address, tonic::Status>>(32);
            let mut request_stream = request.into_inner();

            tokio::spawn(async move {
                while let Some(address_result) = request_stream.next().await {
                    if channel_tx.send(address_result).await.is_err() {
                        break;
                    }
                }
                drop(channel_tx);
            });
            let address_stream = AddressStream::new(channel_rx);

            Ok(tonic::Response::new(
                self.service_subscriber
                    .inner_ref()
                    .get_taddress_balance_stream(address_stream)
                    .await
                    .map_err(Into::into)?,
            ))
        })
    }

    /// Server streaming response type for the GetMempoolTx method.
    #[doc = "Server streaming response type for the GetMempoolTx method."]
    type GetMempoolTxStream = std::pin::Pin<Box<CompactTransactionStream>>;

    /// Server streaming response type for the GetMempoolStream method.
    #[doc = "Server streaming response type for the GetMempoolStream method."]
    type GetMempoolStreamStream = std::pin::Pin<Box<RawTransactionStream>>;

    /// Server streaming response type for the GetSubtreeRoots method.
    #[doc = " Server streaming response type for the GetSubtreeRoots method."]
    type GetSubtreeRootsStream = std::pin::Pin<Box<SubtreeRootReplyStream>>;

    /// Server streaming response type for the GetAddressUtxosStream method.
    #[doc = "Server streaming response type for the GetAddressUtxosStream method."]
    type GetAddressUtxosStreamStream = std::pin::Pin<Box<UtxoReplyStream>>;
}
