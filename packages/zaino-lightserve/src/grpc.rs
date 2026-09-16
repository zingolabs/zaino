//! The `CompactTxStreamer` gRPC service, backed by the [`LightServe`] handler.
//!
//! `CompactTxStreamer` is generated from the lightwalletd proto and is a fixed,
//! monolithic contract: every method must be implemented. The two the handler
//! covers (`GetLatestBlock`, `SendTransaction`) are wired; the rest return
//! `Status::unimplemented` until their handler methods exist. Implemented on a
//! wrapper (not `LightServe` itself) so the handler stays a pure profile handler
//! and there is no inherent/trait method-name clash.

use futures::stream::BoxStream;
use tonic::{Request, Response, Status};

use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamer;
use zaino_proto::proto::service::{
    Address, AddressList, Balance, BlockId, BlockRange, ChainSpec, Duration, Empty,
    GetAddressUtxosArg, GetAddressUtxosReply, GetAddressUtxosReplyList, GetMempoolTxRequest,
    GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction, SendResponse, SubtreeRoot,
    TransparentAddressBlockFilter, TreeState, TxFilter,
};
use zaino_service::LightServeService;

use crate::error::ServeError;
use crate::LightServe;

/// A server-streaming response type. Boxed because the stub methods never
/// construct it (they return `unimplemented`); real streaming lands per method.
type ServerStream<T> = BoxStream<'static, Result<T, Status>>;

/// The `CompactTxStreamer` service over a light-serve handler.
#[derive(Clone)]
pub struct GrpcService<S: LightServeService + Clone> {
    handler: LightServe<S>,
}

impl<S: LightServeService + Clone> GrpcService<S> {
    /// Wrap a light-serve handler as the gRPC service.
    pub fn new(handler: LightServe<S>) -> Self {
        Self { handler }
    }
}

/// Map a light-serve error onto a gRPC status.
fn to_status(err: ServeError) -> Status {
    match err {
        ServeError::NoBlocks => Status::unavailable("no blocks available yet"),
        ServeError::Unavailable(t) => Status::unavailable(t.to_string()),
    }
}

/// The single message for a method whose handler is not built yet.
fn unimplemented(method: &str) -> Status {
    Status::unimplemented(format!("{method} not served yet"))
}

#[tonic::async_trait]
impl<S: LightServeService + Clone + 'static> CompactTxStreamer for GrpcService<S> {
    // --- wired ---

    async fn get_latest_block(
        &self,
        _request: Request<ChainSpec>,
    ) -> Result<Response<BlockId>, Status> {
        self.handler
            .get_latest_block()
            .await
            .map(Response::new)
            .map_err(to_status)
    }

    async fn send_transaction(
        &self,
        request: Request<RawTransaction>,
    ) -> Result<Response<SendResponse>, Status> {
        Ok(Response::new(
            self.handler.send_transaction(request.into_inner()).await,
        ))
    }

    // --- not yet served (unary) ---

    async fn get_block(&self, _r: Request<BlockId>) -> Result<Response<CompactBlock>, Status> {
        Err(unimplemented("get_block"))
    }
    async fn get_block_nullifiers(
        &self,
        _r: Request<BlockId>,
    ) -> Result<Response<CompactBlock>, Status> {
        Err(unimplemented("get_block_nullifiers"))
    }
    async fn get_transaction(
        &self,
        _r: Request<TxFilter>,
    ) -> Result<Response<RawTransaction>, Status> {
        Err(unimplemented("get_transaction"))
    }
    async fn get_taddress_balance(
        &self,
        _r: Request<AddressList>,
    ) -> Result<Response<Balance>, Status> {
        Err(unimplemented("get_taddress_balance"))
    }
    async fn get_taddress_balance_stream(
        &self,
        _r: Request<tonic::Streaming<Address>>,
    ) -> Result<Response<Balance>, Status> {
        Err(unimplemented("get_taddress_balance_stream"))
    }
    async fn get_tree_state(&self, _r: Request<BlockId>) -> Result<Response<TreeState>, Status> {
        Err(unimplemented("get_tree_state"))
    }
    async fn get_latest_tree_state(
        &self,
        _r: Request<Empty>,
    ) -> Result<Response<TreeState>, Status> {
        Err(unimplemented("get_latest_tree_state"))
    }
    async fn get_address_utxos(
        &self,
        _r: Request<GetAddressUtxosArg>,
    ) -> Result<Response<GetAddressUtxosReplyList>, Status> {
        Err(unimplemented("get_address_utxos"))
    }
    async fn get_lightd_info(&self, _r: Request<Empty>) -> Result<Response<LightdInfo>, Status> {
        Err(unimplemented("get_lightd_info"))
    }
    async fn ping(&self, _r: Request<Duration>) -> Result<Response<PingResponse>, Status> {
        Err(unimplemented("ping"))
    }

    // --- not yet served (server-streaming) ---

    type GetBlockRangeStream = ServerStream<CompactBlock>;
    async fn get_block_range(
        &self,
        _r: Request<BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeStream>, Status> {
        Err(unimplemented("get_block_range"))
    }

    type GetBlockRangeNullifiersStream = ServerStream<CompactBlock>;
    async fn get_block_range_nullifiers(
        &self,
        _r: Request<BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeNullifiersStream>, Status> {
        Err(unimplemented("get_block_range_nullifiers"))
    }

    type GetTaddressTxidsStream = ServerStream<RawTransaction>;
    async fn get_taddress_txids(
        &self,
        _r: Request<TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTxidsStream>, Status> {
        Err(unimplemented("get_taddress_txids"))
    }

    type GetTaddressTransactionsStream = ServerStream<RawTransaction>;
    async fn get_taddress_transactions(
        &self,
        _r: Request<TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTransactionsStream>, Status> {
        Err(unimplemented("get_taddress_transactions"))
    }

    type GetMempoolTxStream = ServerStream<CompactTx>;
    async fn get_mempool_tx(
        &self,
        _r: Request<GetMempoolTxRequest>,
    ) -> Result<Response<Self::GetMempoolTxStream>, Status> {
        Err(unimplemented("get_mempool_tx"))
    }

    type GetMempoolStreamStream = ServerStream<RawTransaction>;
    async fn get_mempool_stream(
        &self,
        _r: Request<Empty>,
    ) -> Result<Response<Self::GetMempoolStreamStream>, Status> {
        Err(unimplemented("get_mempool_stream"))
    }

    type GetSubtreeRootsStream = ServerStream<SubtreeRoot>;
    async fn get_subtree_roots(
        &self,
        _r: Request<GetSubtreeRootsArg>,
    ) -> Result<Response<Self::GetSubtreeRootsStream>, Status> {
        Err(unimplemented("get_subtree_roots"))
    }

    type GetAddressUtxosStreamStream = ServerStream<GetAddressUtxosReply>;
    async fn get_address_utxos_stream(
        &self,
        _r: Request<GetAddressUtxosArg>,
    ) -> Result<Response<Self::GetAddressUtxosStreamStream>, Status> {
        Err(unimplemented("get_address_utxos_stream"))
    }
}
