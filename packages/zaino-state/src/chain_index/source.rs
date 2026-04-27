//! Traits and types for the blockchain source thats serves zaino, commonly a validator connection.

use std::{error::Error, str::FromStr as _, sync::Arc};

use crate::chain_index::{
    types::{BlockHash, TransactionHash},
    ShieldedPool,
};
use async_trait::async_trait;
use futures::{future::join, TryFutureExt as _};
use incrementalmerkletree::frontier::CommitmentTree;
use tower::{Service, ServiceExt as _};
use zaino_common::Network;
use zaino_fetch::jsonrpsee::{
    connector::{JsonRpSeeConnector, RpcRequestError},
    response::{GetBlockError, GetBlockResponse, GetTransactionResponse, GetTreestateResponse},
};
use zcash_primitives::merkle_tree::{read_commitment_tree, write_commitment_tree};
use zebra_chain::{
    block::TryIntoHeight, serialization::ZcashDeserialize, subtree::NoteCommitmentSubtreeIndex,
};
use zebra_state::{HashOrHeight, ReadRequest, ReadResponse, ReadStateService};

#[cfg(test)]
pub(crate) mod mockchain_source;

pub mod validator_connector;
pub use validator_connector::*;

/// A trait for accessing blockchain data from different backends.
#[async_trait]
pub trait BlockchainSource: Clone + Send + Sync + 'static {
    /// Returns a best-chain block by hash or height
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>>;

    /// Returns the block commitment tree data by hash
    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )>;

    /// Returns the sapling and orchard treestate by hash
    async fn get_treestate(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)>;

    /// Returns the complete list of txids currently in the mempool.
    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>>;

    /// Returns the transaction by txid
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    >;

    /// Returns the hash of the block at the tip of the best chain.
    async fn get_best_block_hash(&self)
        -> BlockchainSourceResult<Option<zebra_chain::block::Hash>>;

    /// Returns the height of the block at the tip of the best chain.
    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>>;

    /// Get a listener for new nonfinalized blocks,
    /// if supported
    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn Error + Send + Sync>,
    >;

    /// Gets the subtree roots of a given pool and the end heights of each root,
    /// starting at the provided index, up to an optional maximum number of roots.
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>>;
}

/// An error originating from a blockchain source.
#[derive(Debug, thiserror::Error)]
pub enum BlockchainSourceError {
    /// Unrecoverable error.
    // TODO: Add logic for handling recoverable errors if any are identified
    // one candidate may be ephemerable network hiccoughs
    #[error("critical error in backing block source: {0}")]
    Unrecoverable(String),
}

/// Error type returned when invalid data is returned by the validator.
#[derive(thiserror::Error, Debug)]
#[error("data from validator invalid: {0}")]
pub struct InvalidData(String);

pub(crate) type BlockchainSourceResult<T> = Result<T, BlockchainSourceError>;

/// The location of a transaction returned by
/// [BlockchainSource::get_transaction]
#[derive(Debug, Clone)]
pub enum GetTransactionLocation {
    // get_transaction can get the height of the block
    // containing the transaction if it's on the best
    // chain, but cannot reliably if it isn't.
    //
    /// The transaction is in the best chain,
    /// the block height is returned
    BestChain(zebra_chain::block::Height),
    /// The transaction is on a non-best chain
    NonbestChain,
    /// The transaction is in the mempool
    Mempool,
}
