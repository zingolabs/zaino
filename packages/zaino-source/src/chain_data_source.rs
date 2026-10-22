//! [`ChainDataSource`]: every question Zaino asks a trusted validator's RPC, one port
//!
//! - `Err(QueryError::Domain(_))` = the validator's answer (never worth asking again)
//! - `Err(QueryError::Unsupported)` = it has no such method (an older node: ask another)
//! - `Err(QueryError::NonDomain(_))` = no answer (unreachable, timed out, undecodable)
//! - batched questions: one `Result` per item inside the batch's own `NonDomainError`

use std::future::Future;

use zaino_primitives::types::{Block, TransactionLocation};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::{BlockHash, TxId};

use crate::{BlockchainInfo, NodeRelease, NonDomainError, PeerInfo, QueryError};

/// Trusted validator's RPC, as every Zaino consumer asks it
///
/// - production = `ZcashLegacyRpcClient` or `ZebraIndexerGrpcClient` (`zainod`'s `validator_client`)
/// - tests = `ZcashLegacyRpcClient` against `testing::MockValidator`
pub trait ChainDataSource: Send + Sync + 'static {
    /// `getblock <hash> 0`: best-chain block `hash`, decoded from consensus bytes
    fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockByHashError>>> + Send;

    /// `getblock "<h>" 0` per height, batched: best-chain blocks, `heights` order, decoded
    ///
    /// - any height absent = `HeightNotFound` for it (batch = all or nothing)
    fn get_blocks_by_height(
        &self,
        heights: &[BlockHeight],
    ) -> impl Future<Output = Result<Vec<Block>, QueryError<GetAtHeightError>>> + Send;

    /// `getblockheader <height> false` per height, batched: ancestry + raw headers, no bodies
    ///
    /// - one item per height, `heights` order
    fn get_block_links(
        &self,
        heights: &[BlockHeight],
    ) -> impl Future<Output = Result<BlockLinks, NonDomainError>> + Send;

    /// Poll in one round trip (the poll composition's one home)
    ///
    /// - `getblockchaininfo` + `getrawmempool true` + `getblockhash <h>` per `holds` height
    /// - with `metadata`: `getpeerinfo` + `getinfo` + `getdeprecationinfo` too ([`Telemetry`])
    /// - `Err` = no tip read (transport, or the info unparseable)
    fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[BlockHeight],
    ) -> impl Future<Output = Result<PollReading, NonDomainError>> + Send;

    /// `getrawtransaction <txid> 0` per listed mempool transaction, batched by `encoded_len`
    ///
    /// - one item per entry, `listed` order
    fn get_raw_mempool_transactions(
        &self,
        listed: &[MempoolListed],
    ) -> impl Future<Output = Result<RawMempoolTransactions, NonDomainError>> + Send;

    /// `getrawtransaction <txid> 1`: bytes + where it was found (mined vs mempool)
    fn get_transaction(
        &self,
        txid: TxId,
    ) -> impl Future<Output = Result<TransactionResponse, QueryError<GetTransactionError>>> + Send;

    /// `sendrawtransaction`: the one write
    ///
    /// - not idempotent (an error ≠ proof an earlier attempt went unaccepted)
    fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> impl Future<Output = Result<TxId, QueryError<SendRawTransactionError>>> + Send;

    /// `checkadmission`: `sendrawtransaction`'s verdict, nothing admitted or relayed
    ///
    /// - zebra fork only (zcashd + stock zebrad: `Unsupported`)
    fn check_admission(
        &self,
        transaction: Vec<u8>,
    ) -> impl Future<Output = Result<TxId, QueryError<SendRawTransactionError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAtHeightError {
    #[error("no block at height {0}")]
    HeightNotFound(BlockHeight),
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockByHashError {
    #[error("block not found: {0}")]
    NotFound(BlockHash),
}

/// Best-chain block at a height: its consensus header bytes as served
///
/// - undecoded, unhashed (the header chain decodes once, on receipt)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLink {
    pub header: Vec<u8>,
}

pub type BlockLinks = Vec<Result<BlockLink, GetAtHeightError>>;

/// Poll batch answers (`info` = tip the listing is tagged with)
///
/// - `held[i]` = its best-chain hash at `holds[i]`
/// - `HeightNotFound` = above its tip
/// - `NonDomain` = that item unanswered (rest of the poll stands)
#[derive(Debug)]
pub struct PollReading {
    pub info: BlockchainInfo,
    pub listing: Result<Vec<MempoolListed>, GetMempoolListingError>,
    pub held: Vec<Result<BlockHash, QueryError<GetAtHeightError>>>,
    pub metadata: Option<MetadataReading>,
}

/// Metadata poll: what answered + each failed read, named (a failure never fails the poll)
#[derive(Debug)]
pub struct MetadataReading {
    pub answered: Telemetry,
    pub failed: Vec<(&'static str, NonDomainError)>,
}

/// One validator's telemetry (`None` = unread: never answered, or its read failed)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Telemetry {
    pub peers: Option<Vec<PeerInfo>>,
    pub release: Option<NodeRelease>,
}

impl Telemetry {
    /// `newer`'s answered reads over these (an unread one keeps the last)
    pub fn absorb(&mut self, newer: Telemetry) {
        let Telemetry { peers, release } = newer;
        self.peers = peers.or(self.peers.take());
        self.release = release.or(self.release.take());
    }
}

/// - `Unavailable` = no mempool on the node (no `getrawmempool`): stop asking
/// - `Inactive` = off until the node reaches the network tip (its own tip still valid)
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetMempoolListingError {
    #[error("mempool unavailable")]
    Unavailable,
    #[error("mempool inactive: validator catching up to the network tip")]
    Inactive,
}

/// `fee` = what the validator computed admitting it (its UTXO set, not ours)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolListed {
    pub txid: TxId,
    pub fee: Zatoshis,
    pub encoded_len: u32,
}

/// `NotFound` = mined or evicted between listing and fetch (normal race: skip it)
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetRawMempoolTransactionError {
    #[error("transaction {0} not in mempool")]
    NotFound(TxId),
}

pub type RawMempoolTransactions = Vec<Result<Vec<u8>, GetRawMempoolTransactionError>>;

#[derive(Debug, Clone)]
pub struct TransactionResponse {
    pub bytes: Vec<u8>,
    pub location: TransactionLocation,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetTransactionError {
    #[error("transaction not found: {0}")]
    NotFound(TxId),
}

/// Verdict on the transaction, as the validator's JSON-RPC error spelled it (`code` = its own)
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SendRawTransactionError {
    #[error("malformed transaction: {0}")]
    Malformed(String),
    #[error("rejected by validator ({code}): {message}")]
    Rejected { code: i32, message: String },
}

impl SendRawTransactionError {
    /// JSON-RPC code of the verdict (`Malformed` = `-22`, `RPC_DESERIALIZATION_ERROR`)
    pub fn code(&self) -> i32 {
        match self {
            Self::Malformed(_) => -22,
            Self::Rejected { code, .. } => *code,
        }
    }

    /// Validator's reason, verbatim
    pub fn message(&self) -> &str {
        match self {
            Self::Malformed(message) | Self::Rejected { message, .. } => message,
        }
    }
}
