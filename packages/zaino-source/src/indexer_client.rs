//! [`ZebraIndexerGrpcClient`]: blocks over zebrad's indexer gRPC, every other question over its JSON-RPC
//!
//! - `GetBlockRange` / `GetBlock` = stored consensus bytes as protobuf `bytes` (no hex, no JSON),
//!   decoded by the same [`decode::decode_block`] as JSON-RPC
//! - `Bytes` = slice of the received frame
//! - HTTP/2: streams multiplexed over a few connections (zebrad admits 20 streams per one),
//!   requests rotated across them
//! - the rest (poll, headers, mempool, transactions) = [`ZcashLegacyRpcClient`]'s (no indexer
//!   equivalent)

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tonic::transport::Channel;
use tonic::{Code, Status};
use zaino_primitives::types::Block;
use zaino_proto::proto::zebra_indexer::indexer_client::IndexerClient;
use zaino_proto::proto::zebra_indexer::{BlockRangeRequest, BlockRequest};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{BlockHash, TxId};

use crate::indexer_endpoint::{zebra_indexer_endpoint, ZebraIndexerTls};
use crate::{
    decode, metrics, BlockLinks, ChainDataSource, EndpointError, FailureMode, GetAtHeightError,
    GetBlockByHashError, LinkLimits, MempoolListed, NonDomainError, PollReading, QueryError,
    RawMempoolTransactions, SendRawTransactionError, Timeouts, TransactionResponse,
    ZcashLegacyRpcClient,
};

/// zebrad's `MAX_CONCURRENT_STREAMS` per indexer connection (`zebra-rpc/src/indexer/server.rs`)
const ZEBRAD_STREAMS_PER_CONNECTION: u32 = 20;

/// Receive window per stream: a whole block (consensus max 2,000,000 bytes) before the sender
/// waits (h2's 64 KiB default = a round trip per 64 KiB of block)
const STREAM_WINDOW: u32 = 4 << 20;

/// Receive window per connection: every stream's at once
const CONNECTION_WINDOW: u32 = STREAM_WINDOW * ZEBRAD_STREAMS_PER_CONNECTION;

/// Largest message decoded: one block + its hash
const MAX_MESSAGE_BYTES: usize = 4 << 20;

/// Idle-connection ping cadence + pong deadline (a dead peer found without a request failing)
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// One validator: blocks from its indexer, the rest from its JSON-RPC
///
/// - Single attempt per call (retries, concurrency = the caller's balancer)
/// - `read` = silence before a call fails (per message, never a total: a range takes as long as
///   it takes)
pub struct ZebraIndexerGrpcClient {
    rpc: ZcashLegacyRpcClient,
    connections: Vec<IndexerClient<Channel>>,
    next: AtomicUsize,
    read: Duration,
}

impl ZebraIndexerGrpcClient {
    /// Blocks from the zebra indexer at `zebra_indexer_address` (`host:port`, `tls` = mutual TLS),
    /// the rest through `rpc` (unprobed)
    ///
    /// - connections = `max_connections` streams at zebrad's 20 per connection
    /// - Each connection opens on its first call (inside a tokio runtime)
    pub fn new(
        rpc: ZcashLegacyRpcClient,
        zebra_indexer_address: &str,
        tls: Option<&ZebraIndexerTls>,
        timeouts: Timeouts,
        limits: LinkLimits,
    ) -> Result<Self, EndpointError> {
        let endpoint = zebra_indexer_endpoint(zebra_indexer_address, tls, timeouts)?
            .initial_stream_window_size(STREAM_WINDOW)
            .initial_connection_window_size(CONNECTION_WINDOW)
            .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
            .keep_alive_timeout(KEEPALIVE_TIMEOUT);
        let count = limits.max_connections.get().div_ceil(ZEBRAD_STREAMS_PER_CONNECTION);
        let connect = |_| {
            let client = IndexerClient::new(endpoint.connect_lazy());
            client.max_decoding_message_size(MAX_MESSAGE_BYTES)
        };
        let connections = (0..count).map(connect).collect();
        Ok(Self { rpc, connections, next: AtomicUsize::new(0), read: timeouts.read })
    }

    /// Round-robin over `connections`
    fn zebra_indexer(&self) -> IndexerClient<Channel> {
        let turn = self.next.fetch_add(1, Ordering::Relaxed);
        self.connections[turn % self.connections.len()].clone()
    }

    /// `run` = consecutive ascending heights, one `GetBlockRange`, each block decoded into `blocks`
    async fn block_range(
        &self,
        run: &[BlockHeight],
        blocks: &mut Vec<Block>,
    ) -> Result<(), QueryError<GetAtHeightError>> {
        let (Some(first), Some(last)) = (run.first(), run.last()) else { return Ok(()) };
        let request = BlockRangeRequest { start: u32::from(*first), end: u32::from(*last) };
        let mut zebra_indexer = self.zebra_indexer();
        let opened = self.within(zebra_indexer.get_block_range(request)).await?;
        let opened = opened.map_err(|status| QueryError::NonDomain(failed(status)))?;
        let mut stream = opened.into_inner();
        for height in run {
            let message = self.within(stream.message()).await?;
            let block = match message {
                Ok(Some(block)) => block,
                Ok(None) => {
                    let ended = format!("stream ended before height {}", u32::from(*height));
                    return Err(QueryError::NonDomain(NonDomainError::new(
                        FailureMode::Parse,
                        ended,
                    )));
                }
                Err(status) if status.code() == Code::NotFound => {
                    return Err(QueryError::Domain(GetAtHeightError::HeightNotFound(*height)));
                }
                Err(status) => return Err(QueryError::NonDomain(failed(status))),
            };
            self.rpc.rpc.received(block.data.len()).await;
            blocks.push(decode::served_block(&block.data)?);
        }
        Ok(())
    }

    /// `step` cut at [`Self::read`] of silence
    async fn within<T, E>(&self, step: impl Future<Output = T>) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        tokio::time::timeout(self.read, step).await.map_err(|_| {
            let silent = format!("indexer silent for {:?}", self.read);
            QueryError::NonDomain(NonDomainError::new(FailureMode::Timeout, silent))
        })
    }

    /// One indexer call timed + counted as `method`
    async fn measured<T, E>(
        &self,
        method: &str,
        call: impl Future<Output = Result<T, QueryError<E>>>,
    ) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        let name = self.rpc.rpc.name();
        let _in_flight = metrics::InFlight::start(name);
        let started = Instant::now();
        let settled = call.await;
        let failure = match &settled {
            Err(QueryError::NonDomain(error)) => Some(error.mode),
            _ => None,
        };
        metrics::call_settled(name, method, started.elapsed(), failure);
        settled
    }
}

/// A status other than the port's own answers (`NOT_FOUND` is handled by each caller)
fn failed(status: Status) -> NonDomainError {
    let mode = match status.code() {
        Code::Unavailable | Code::Cancelled => FailureMode::Connection,
        Code::DeadlineExceeded => FailureMode::Timeout,
        Code::Unauthenticated | Code::PermissionDenied => FailureMode::Auth,
        code => FailureMode::GrpcStatus(code as i32),
    };
    NonDomainError::from_cause(mode, status)
}

/// `heights` cut where one is not the previous + 1 (each run = one `GetBlockRange`)
fn consecutive_runs(heights: &[BlockHeight]) -> impl Iterator<Item = &[BlockHeight]> {
    heights.chunk_by(|a, b| u32::from(*a).checked_add(1) == Some(u32::from(*b)))
}

impl ChainDataSource for ZebraIndexerGrpcClient {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.measured("GetBlock", async {
            let mut display = hash.0;
            display.reverse();
            let request = BlockRequest { hash_or_height: display.to_vec() };
            let mut zebra_indexer = self.zebra_indexer();
            match self.within(zebra_indexer.get_block(request)).await? {
                Ok(block) => {
                    let block = block.into_inner();
                    self.rpc.rpc.received(block.data.len()).await;
                    decode::served_block(&block.data)
                }
                Err(status) if status.code() == Code::NotFound => {
                    Err(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
                }
                Err(status) => Err(QueryError::NonDomain(failed(status))),
            }
        })
        .await
    }

    async fn get_blocks_by_height(
        &self,
        heights: &[BlockHeight],
    ) -> Result<Vec<Block>, QueryError<GetAtHeightError>> {
        let mut blocks = Vec::with_capacity(heights.len());
        for run in consecutive_runs(heights) {
            self.measured("GetBlockRange", self.block_range(run, &mut blocks)).await?;
        }
        Ok(blocks)
    }

    async fn get_block_links(&self, heights: &[BlockHeight]) -> Result<BlockLinks, NonDomainError> {
        self.rpc.get_block_links(heights).await
    }

    async fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[BlockHeight],
    ) -> Result<PollReading, NonDomainError> {
        self.rpc.get_poll_reading(metadata, holds).await
    }

    async fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> Result<RawMempoolTransactions, NonDomainError> {
        self.rpc.get_raw_mempool_transactions(listed).await
    }

    async fn get_transaction(
        &self,
        txid: TxId,
    ) -> Result<TransactionResponse, QueryError<crate::GetTransactionError>> {
        self.rpc.get_transaction(txid).await
    }

    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TxId, QueryError<SendRawTransactionError>> {
        self.rpc.send_raw_transaction(transaction).await
    }

    async fn check_admission(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TxId, QueryError<crate::CheckAdmissionError>> {
        self.rpc.check_admission(transaction).await
    }
}
