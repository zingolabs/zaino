//! One trait per question a consumer asks a validator, each with its own domain error
//!
//! - `Err(QueryError::Domain(_))` = the validator's answer (never worth asking again)
//! - `Err(QueryError::NonDomain(_))` = no answer (unreachable, timed out, undecodable)
//! - Bounds name only what a consumer asks (fakes implement the same traits)

use std::convert::Infallible;
use std::future::Future;

use zaino_primitives::types::PeerInfo;
use zaino_primitives::types::{
    Block, BlockHash, BlockchainInfo, Height, TransactionId, TransactionLocation, Zatoshis,
};

use crate::QueryError;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockError {
    #[error("no block at height {0}")]
    HeightNotFound(Height),
}

/// `getblock <height> 0`, decoded from consensus bytes
pub trait GetBlock: Send + Sync {
    fn get_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockByHashError {
    #[error("block not found: {0}")]
    NotFound(BlockHash),
}

/// `getblock <hash> 0`, decoded from consensus bytes
pub trait GetBlockByHash: Send + Sync {
    fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockByHashError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockLinkError {
    #[error("no block at height {0}")]
    HeightNotFound(Height),
}

/// Best-chain block at a height: its hash (from the header bytes) + its parent's
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockLink {
    pub hash: BlockHash,
    pub prev_hash: BlockHash,
}

/// `getblockheader <height> false`: ancestry without the block body
pub trait GetBlockLink: Send + Sync {
    fn get_block_link(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<BlockLink, QueryError<GetBlockLinkError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetChainTipError {
    #[error("validator not ready")]
    NotReady,
}

/// `getbestblockheightandhash` (one call: hash and height from the same tip)
pub trait GetChainTip: Send + Sync {
    fn get_chain_tip(
        &self,
    ) -> impl Future<Output = Result<(BlockHash, Height), QueryError<GetChainTipError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockchainInfoError {
    #[error("validator not ready")]
    NotReady,
}

/// `getblockchaininfo`, incl. the upgrade schedule (a consensus input, not telemetry)
pub trait GetBlockchainInfo: Send + Sync {
    fn get_blockchain_info(
        &self,
    ) -> impl Future<Output = Result<BlockchainInfo, QueryError<GetBlockchainInfoError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetMempoolListingError {
    /// Node exposes no mempool (`-32601`): about the node, so stop asking
    #[error("mempool unavailable")]
    Unavailable,
    /// Mempool off until the node reaches the network tip (its own tip still valid)
    #[error("mempool inactive: validator catching up to the network tip")]
    Inactive,
}

/// One mempool entry: its fee = what the validator computed admitting it (its UTXO set, not ours)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolListed {
    pub txid: TransactionId,
    pub fee: Zatoshis,
}

/// `getrawmempool true`
pub trait GetMempoolListing: Send + Sync {
    fn get_mempool_listing(
        &self,
    ) -> impl Future<Output = Result<Vec<MempoolListed>, QueryError<GetMempoolListingError>>> + Send;
}

/// Source's own tip + its estimate of the network's (`estimated_height` = an estimate even when
/// synced: telemetry, never a vote)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTip {
    pub hash: BlockHash,
    pub height: Height,
    pub estimated_height: Height,
}

/// Tip of the source that served the mempool listing (tags a listing coherently)
///
/// - Same source as [`GetMempoolListing`], never a cheaper tip elsewhere
/// - `Infallible` domain: `getblockchaininfo` returns a tip or fails in transport
pub trait GetMempoolSourceTip: Send + Sync {
    fn get_mempool_source_tip(
        &self,
    ) -> impl Future<Output = Result<SourceTip, QueryError<Infallible>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetRawMempoolTransactionError {
    /// Mined or evicted between listing and fetch (normal race: skip it)
    #[error("transaction {0} not in mempool")]
    NotFound(TransactionId),
}

/// `getrawtransaction <txid> 0` for a listed mempool transaction (same source as the listing)
pub trait GetRawMempoolTransaction: Send + Sync {
    fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetPeerInfoError {
    #[error("validator not ready")]
    NotReady,
}

/// `getpeerinfo`: the validator's peers (empty = isolated, not an error)
pub trait GetPeerInfo: Send + Sync {
    fn get_peer_info(
        &self,
    ) -> impl Future<Output = Result<Vec<PeerInfo>, QueryError<GetPeerInfoError>>> + Send;
}

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

/// `getrawtransaction <txid> 1` (bytes + where it was found)
pub trait GetTransaction: Send + Sync {
    fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<Output = Result<TransactionResponse, QueryError<GetTransactionError>>> + Send;
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SendRawTransactionError {
    #[error("malformed transaction: {0}")]
    Malformed(String),
    #[error("rejected by validator: {0}")]
    Rejected(String),
}

/// `sendrawtransaction`: the one mutating call
///
/// - Not idempotent: an error does not prove the transaction was not accepted earlier
pub trait SendRawTransaction: Send + Sync {
    fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> impl Future<Output = Result<TransactionId, QueryError<SendRawTransactionError>>> + Send;
}
