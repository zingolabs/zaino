//! The `CompactTxStreamer` gRPC service, backed by the [`LightServe`] handler.
//!
//! `CompactTxStreamer` is generated from the lightwalletd proto and is a fixed,
//! monolithic contract: every method must be implemented. The compact-block
//! serving path (`GetLatestBlock`, `GetBlock`, `GetBlockRange`,
//! `GetLightdInfo`, `SendTransaction`) is wired; the rest return
//! `Status::unimplemented` until their handler methods exist. Implemented on a
//! wrapper (not `LightServe` itself) so the handler stays a pure profile handler
//! and there is no inherent/trait method-name clash.

use futures::stream::{BoxStream, StreamExt};
use tonic::{Request, Response, Status};

use zaino_core::{BlockHash, BlockRef, Height, HeightRange, ShieldedPool};

use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamer;
use zaino_proto::proto::service::{
    Address, AddressList, Balance, BlockId, BlockRange, ChainSpec, Duration, Empty,
    GetAddressUtxosArg, GetAddressUtxosReply, GetAddressUtxosReplyList, GetMempoolTxRequest,
    GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction, SendResponse, ShieldedProtocol,
    SubtreeRoot, TransparentAddressBlockFilter, TreeState, TxFilter,
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

/// Map a light-serve error onto a gRPC status, preserving its kind: a
/// serviceability fact (`unavailable` / `failed_precondition`), a transient
/// snapshot failure (`unavailable`), and an unrecoverable backend failure
/// (`internal`) are distinct wire outcomes.
fn to_status(err: ServeError) -> Status {
    match err {
        ServeError::NoBlocks => Status::unavailable("no blocks available yet"),
        ServeError::NotServiceable(cap) => {
            Status::failed_precondition(format!("not serviceable yet: {cap:?}"))
        }
        ServeError::Unavailable(t) => Status::unavailable(t.to_string()),
        ServeError::Internal(msg) => Status::internal(msg),
    }
}

/// The single message for a method whose handler is not built yet.
fn unimplemented(method: &str) -> Status {
    Status::unimplemented(format!("{method} not served yet"))
}

/// Wire -> domain for a block reference: a 32-byte hash if present, else the
/// height. This is the external-input validation step (`invalid_argument` on a
/// malformed hash or an out-of-range height), owned by the adapter.
fn block_ref_from_wire(id: BlockId) -> Result<BlockRef, Status> {
    if id.hash.is_empty() {
        return Ok(BlockRef::Height(height_from_wire(id.height)?));
    }
    let bytes: [u8; 32] = id
        .hash
        .try_into()
        .map_err(|_| Status::invalid_argument("block hash must be 32 bytes"))?;
    Ok(BlockRef::Hash(BlockHash::from(bytes)))
}

/// Wire -> domain for an inclusive height range. Both bounds must be present and
/// in range (`invalid_argument` otherwise).
fn height_range_from_wire(range: BlockRange) -> Result<HeightRange, Status> {
    let start = range
        .start
        .ok_or_else(|| Status::invalid_argument("block range requires a start"))?;
    let end = range
        .end
        .ok_or_else(|| Status::invalid_argument("block range requires an end"))?;
    Ok(HeightRange {
        start: height_from_wire(start.height)?,
        end: height_from_wire(end.height)?,
    })
}

/// Wire `u64` height -> domain `Height`, rejecting out-of-range values.
fn height_from_wire(height: u64) -> Result<Height, Status> {
    let narrowed =
        u32::try_from(height).map_err(|_| Status::invalid_argument("height out of range"))?;
    Height::try_from(narrowed).map_err(|_| Status::invalid_argument("height out of range"))
}

/// Wire -> domain height for a treestate query. The domain treestate read is
/// height-addressed, so a hash-only request is rejected (`invalid_argument`)
/// rather than silently resolved — resolving hash -> height is not this read's
/// job. A request carrying a height (hash empty) takes the common wallet path.
fn tree_state_height_from_wire(id: BlockId) -> Result<Height, Status> {
    if id.hash.is_empty() {
        height_from_wire(id.height)
    } else {
        Err(Status::invalid_argument(
            "get_tree_state by block hash is not supported; request by height",
        ))
    }
}

/// Wire -> domain for a subtree-roots query: the shielded protocol, the start
/// index, and the entry limit (`0` meaning "all", mapped to `None`). The
/// external-input validation step — an unknown protocol or an index/limit past
/// the domain's `u16` bound is `invalid_argument`. Kept here beside the other
/// wire -> domain input helpers (which return `Status`), so `wire.rs` stays a
/// pure domain -> wire module with no tonic dependency.
fn subtree_roots_query_from_wire(
    arg: GetSubtreeRootsArg,
) -> Result<(ShieldedPool, u16, Option<u16>), Status> {
    let pool = match ShieldedProtocol::try_from(arg.shielded_protocol) {
        Ok(ShieldedProtocol::Sapling) => ShieldedPool::Sapling,
        Ok(ShieldedProtocol::Orchard) => ShieldedPool::Orchard,
        Ok(ShieldedProtocol::Ironwood) => ShieldedPool::Ironwood,
        Err(_) => return Err(Status::invalid_argument("unknown shielded protocol")),
    };
    let start_index = u16::try_from(arg.start_index)
        .map_err(|_| Status::invalid_argument("subtree start index out of range"))?;
    let limit = match arg.max_entries {
        0 => None,
        entries => Some(
            u16::try_from(entries)
                .map_err(|_| Status::invalid_argument("subtree max entries out of range"))?,
        ),
    };
    Ok((pool, start_index, limit))
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

    // --- wired: index-only compact-block serving ---

    async fn get_block(&self, r: Request<BlockId>) -> Result<Response<CompactBlock>, Status> {
        let at = block_ref_from_wire(r.into_inner())?;
        match self.handler.get_block(at).await.map_err(to_status)? {
            Some(block) => Ok(Response::new(block)),
            None => Err(Status::not_found("no block at the requested reference")),
        }
    }

    // --- not yet served (unary) ---

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
    async fn get_tree_state(&self, r: Request<BlockId>) -> Result<Response<TreeState>, Status> {
        let height = tree_state_height_from_wire(r.into_inner())?;
        self.handler
            .get_tree_state(height)
            .await
            .map(Response::new)
            .map_err(to_status)
    }
    async fn get_latest_tree_state(
        &self,
        _r: Request<Empty>,
    ) -> Result<Response<TreeState>, Status> {
        self.handler
            .get_latest_tree_state()
            .await
            .map(Response::new)
            .map_err(to_status)
    }
    async fn get_address_utxos(
        &self,
        _r: Request<GetAddressUtxosArg>,
    ) -> Result<Response<GetAddressUtxosReplyList>, Status> {
        Err(unimplemented("get_address_utxos"))
    }
    async fn get_lightd_info(&self, _r: Request<Empty>) -> Result<Response<LightdInfo>, Status> {
        self.handler
            .get_lightd_info()
            .await
            .map(Response::new)
            .map_err(to_status)
    }
    async fn ping(&self, _r: Request<Duration>) -> Result<Response<PingResponse>, Status> {
        Err(unimplemented("ping"))
    }

    // --- wired: index-only compact-block streaming ---

    type GetBlockRangeStream = ServerStream<CompactBlock>;
    async fn get_block_range(
        &self,
        r: Request<BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeStream>, Status> {
        let range = height_range_from_wire(r.into_inner())?;
        let blocks = self
            .handler
            .get_block_range(range)
            .await
            .map_err(to_status)?;
        Ok(Response::new(
            blocks.map(|block| block.map_err(to_status)).boxed(),
        ))
    }

    // --- not yet served (server-streaming) ---

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
        r: Request<GetSubtreeRootsArg>,
    ) -> Result<Response<Self::GetSubtreeRootsStream>, Status> {
        let (pool, start_index, limit) = subtree_roots_query_from_wire(r.into_inner())?;
        let roots = self
            .handler
            .get_subtree_roots(pool, start_index, limit)
            .await
            .map_err(to_status)?;
        // The roots are collected owned, so the stream is `'static`.
        let stream = futures::stream::iter(roots.into_iter().map(Ok));
        Ok(Response::new(stream.boxed()))
    }

    type GetAddressUtxosStreamStream = ServerStream<GetAddressUtxosReply>;
    async fn get_address_utxos_stream(
        &self,
        _r: Request<GetAddressUtxosArg>,
    ) -> Result<Response<Self::GetAddressUtxosStreamStream>, Status> {
        Err(unimplemented("get_address_utxos_stream"))
    }
}
