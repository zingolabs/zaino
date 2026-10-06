//! The `CompactTxStreamer` service, for everything the indexes do not claim.
//!
//! `CompactTxStreamer` is generated from the lightwalletd proto and is a fixed, monolithic
//! contract: every method must exist. The [`Router`](crate::Router) intercepts the methods an
//! enabled index backs and answers them from stored bytes; this is what it falls through to.
//!
//! So the block-serving methods here are **not** stubs awaiting an implementation — they are
//! the answer when no index backs them, which is what an operator who disabled that index has
//! asked for.
//!
//! Served here rather than by an index: `SendTransaction` (a write), `GetTransaction` (a point
//! lookup of consensus data the validator already holds) and `GetLightdInfo` (a question about
//! this server). No index can answer any of them.

use futures::stream::BoxStream;
use tonic::{Request, Response, Status};
use zaino_primitives::types::TransactionId;

use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamer;
use zaino_proto::proto::service::{
    Address, AddressList, Balance, BlockId, BlockRange, ChainSpec, Empty, GetAddressUtxosArg,
    GetAddressUtxosReply, GetAddressUtxosReplyList, GetMempoolTxRequest, GetSubtreeRootsArg,
    LightdInfo, RawTransaction, SendResponse, SubtreeRoot, TransparentAddressBlockFilter,
    TreeState, TxFilter,
};

use crate::validator::{ValidatorHandler, ValidatorPorts};

/// A server-streaming response type. Boxed because the unclaimed methods never construct one.
type ServerStream<T> = BoxStream<'static, Result<T, Status>>;

/// The `CompactTxStreamer` service for the unclaimed methods.
pub struct GrpcService<S> {
    handler: ValidatorHandler<S>,
}

/// Hand-written for the same reason as [`ValidatorHandler`]'s: deriving would demand
/// `S: Clone`, and the validator adapter is not.
impl<S> Clone for GrpcService<S> {
    fn clone(&self) -> Self {
        Self { handler: self.handler.clone() }
    }
}

impl<S: ValidatorPorts> GrpcService<S> {
    pub fn new(handler: ValidatorHandler<S>) -> Self {
        Self { handler }
    }
}

/// The single message for a method nothing serves.
fn unserved(method: &str) -> Status {
    Status::unimplemented(format!("{method} is not served by this node"))
}

/// A block method that reached here because no index claimed it.
fn no_index(method: &str) -> Status {
    Status::unimplemented(format!("{method} needs the compact_block index, which is not enabled"))
}

#[tonic::async_trait]
impl<S: ValidatorPorts> CompactTxStreamer for GrpcService<S> {
    // --- served here: no index can answer these ---

    async fn send_transaction(
        &self,
        request: Request<RawTransaction>,
    ) -> Result<Response<SendResponse>, Status> {
        Ok(Response::new(self.handler.send_transaction(request.into_inner()).await))
    }

    async fn get_lightd_info(&self, _r: Request<Empty>) -> Result<Response<LightdInfo>, Status> {
        self.handler.lightd_info().map(Response::new)
    }

    // --- the compact_block index's, when it is enabled ---

    async fn get_latest_block(&self, _r: Request<ChainSpec>) -> Result<Response<BlockId>, Status> {
        Err(no_index("get_latest_block"))
    }

    async fn get_block(&self, _r: Request<BlockId>) -> Result<Response<CompactBlock>, Status> {
        Err(no_index("get_block"))
    }

    type GetBlockRangeStream = ServerStream<CompactBlock>;
    async fn get_block_range(
        &self,
        _r: Request<BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeStream>, Status> {
        Err(no_index("get_block_range"))
    }

    // TODO: REMOVE THIS — deprecated alias of `get_block_range` (pepper-sync still calls it)
    type GetBlockRangeNullifiersStream = ServerStream<CompactBlock>;
    #[allow(deprecated)]
    async fn get_block_range_nullifiers(
        &self,
        _r: Request<BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeNullifiersStream>, Status> {
        Err(no_index("get_block_range_nullifiers"))
    }

    // --- no index backs these yet ---

    /// Forwarded, not indexed — see `validator.rs` for why.
    ///
    /// Only the `hash` arm is answerable. `TxFilter`'s `(block, index)` arm is a positional
    /// locator the validator port does not take, and no index maps it.
    async fn get_transaction(
        &self,
        request: Request<TxFilter>,
    ) -> Result<Response<RawTransaction>, Status> {
        let filter = request.into_inner();

        let txid: [u8; 32] =
            filter.hash.as_slice().try_into().map_err(|_| {
                Status::invalid_argument("txid must be 32 bytes, in protocol order")
            })?;

        self.handler.transaction(TransactionId::from(txid)).await.map(Response::new)
    }
    async fn get_taddress_balance(
        &self,
        _r: Request<AddressList>,
    ) -> Result<Response<Balance>, Status> {
        Err(unserved("get_taddress_balance"))
    }
    async fn get_taddress_balance_stream(
        &self,
        _r: Request<tonic::Streaming<Address>>,
    ) -> Result<Response<Balance>, Status> {
        Err(unserved("get_taddress_balance_stream"))
    }
    async fn get_tree_state(&self, _r: Request<BlockId>) -> Result<Response<TreeState>, Status> {
        Err(unserved("get_tree_state"))
    }
    async fn get_latest_tree_state(
        &self,
        _r: Request<Empty>,
    ) -> Result<Response<TreeState>, Status> {
        Err(unserved("get_latest_tree_state"))
    }
    async fn get_address_utxos(
        &self,
        _r: Request<GetAddressUtxosArg>,
    ) -> Result<Response<GetAddressUtxosReplyList>, Status> {
        Err(unserved("get_address_utxos"))
    }

    type GetTaddressTransactionsStream = ServerStream<RawTransaction>;
    async fn get_taddress_transactions(
        &self,
        _r: Request<TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTransactionsStream>, Status> {
        Err(unserved("get_taddress_transactions"))
    }

    // TODO: REMOVE THIS — deprecated alias of `get_taddress_transactions` (pepper-sync still
    // calls it)
    type GetTaddressTxidsStream = ServerStream<RawTransaction>;
    #[allow(deprecated)]
    async fn get_taddress_txids(
        &self,
        _r: Request<TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTxidsStream>, Status> {
        Err(unserved("get_taddress_txids"))
    }

    type GetMempoolTxStream = ServerStream<CompactTx>;
    async fn get_mempool_tx(
        &self,
        _r: Request<GetMempoolTxRequest>,
    ) -> Result<Response<Self::GetMempoolTxStream>, Status> {
        Err(unserved("get_mempool_tx"))
    }

    type GetMempoolStreamStream = ServerStream<RawTransaction>;
    async fn get_mempool_stream(
        &self,
        _r: Request<Empty>,
    ) -> Result<Response<Self::GetMempoolStreamStream>, Status> {
        Err(unserved("get_mempool_stream"))
    }

    type GetSubtreeRootsStream = ServerStream<SubtreeRoot>;
    async fn get_subtree_roots(
        &self,
        _r: Request<GetSubtreeRootsArg>,
    ) -> Result<Response<Self::GetSubtreeRootsStream>, Status> {
        Err(unserved("get_subtree_roots"))
    }

    type GetAddressUtxosStreamStream = ServerStream<GetAddressUtxosReply>;
    async fn get_address_utxos_stream(
        &self,
        _r: Request<GetAddressUtxosArg>,
    ) -> Result<Response<Self::GetAddressUtxosStreamStream>, Status> {
        Err(unserved("get_address_utxos_stream"))
    }
}
