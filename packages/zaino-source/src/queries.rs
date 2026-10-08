//! [`ChainDataSource`]: every question Zaino asks a trusted validator's RPC, one port
//!
//! - `Err(QueryError::Domain(_))` = the validator's answer (never worth asking again)
//! - `Err(QueryError::NonDomain(_))` = no answer (unreachable, timed out, undecodable)
//! - batched questions: one `Result` per item inside the batch's own `NonDomainError`

use std::future::Future;

use zaino_primitives::types::{
    Block, BlockHash, BlockchainInfo, Height, NodeRelease, PeerInfo, TransactionId,
    TransactionLocation, Zatoshis,
};

use crate::{NonDomainError, QueryError};

/// One trusted validator's RPC, as every Zaino consumer asks it
///
/// - production = `ZebraRpcAdapter`; a test fake answers what its test asks (`unimplemented!()`
///   the rest)
pub trait ChainDataSource: Send + Sync + 'static {
    /// `getblock <hash> 0`, decoded from consensus bytes
    fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockByHashError>>> + Send;

    /// `getblockheader <height> false` per height, batched: ancestry + raw headers, no bodies
    ///
    /// - one item per height, `heights` order
    fn get_block_links(
        &self,
        heights: &[Height],
    ) -> impl Future<Output = Result<BlockLinks, NonDomainError>> + Send;

    /// One poll in one round trip: `getblockchaininfo` + `getrawmempool true` + `getblockhash <h>`
    /// per `holds` height; with `metadata`, `getpeerinfo` + `getinfo` + `getdeprecationinfo` too
    ///
    /// - `Err` = no tip read (transport, or the info unparseable)
    fn get_poll_reading(
        &self,
        metadata: bool,
        holds: &[Height],
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
        txid: TransactionId,
    ) -> impl Future<Output = Result<TransactionResponse, QueryError<GetTransactionError>>> + Send;

    /// `sendrawtransaction`: the one write
    ///
    /// - not idempotent (an error ≠ proof an earlier attempt went unaccepted)
    fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> impl Future<Output = Result<TransactionId, QueryError<SendRawTransactionError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetAtHeightError {
    #[error("no block at height {0}")]
    HeightNotFound(Height),
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

/// One batch's answers (the tip a listing is tagged with)
///
/// - `held[i]` = its best-chain hash at `holds[i]`; `HeightNotFound` = above its tip; `NonDomain`
///   = that item unanswered (the rest of the poll stands)
#[derive(Debug)]
pub struct PollReading {
    pub info: BlockchainInfo,
    pub listing: Result<Vec<MempoolListed>, GetMempoolListingError>,
    pub held: Vec<Result<BlockHash, QueryError<GetAtHeightError>>>,
    pub metadata: Option<MetadataReading>,
}

/// Telemetry reads: each its own outcome (a failure keeps the last, never fails the poll)
#[derive(Debug)]
pub struct MetadataReading {
    pub peers: Result<Vec<PeerInfo>, NonDomainError>,
    pub release: Result<NodeRelease, NonDomainError>,
}

/// - `Unavailable` = no mempool on the node (`-32601`): stop asking
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
    pub txid: TransactionId,
    pub fee: Zatoshis,
    pub encoded_len: u32,
}

/// `NotFound` = mined or evicted between listing and fetch (normal race: skip it)
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetRawMempoolTransactionError {
    #[error("transaction {0} not in mempool")]
    NotFound(TransactionId),
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
    NotFound(TransactionId),
}

/// A verdict on the transaction, as the validator's JSON-RPC error spelled it (`code` = its own)
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

    /// The validator's reason, verbatim
    pub fn message(&self) -> &str {
        match self {
            Self::Malformed(message) | Self::Rejected { message, .. } => message,
        }
    }
}
