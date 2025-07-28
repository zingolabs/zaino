use std::{
    collections::HashMap,
    fmt::{Debug, Display},
    future::Future,
};

use super::types::{self, ChainBlock};
use futures::Stream;
use zebra_state::HashOrHeight;

/// The interface to the chain index
pub trait ChainIndex: Sized {
    /// A snapshot of the nonfinalized state, needed for atomic access
    type Snapshot: NonFinalizedSnapshot;

    /// How it can fail
    type Error: std::error::Error;
    /// Takes a snapshot of the non_finalized state. All NFS-interfacing query
    /// methods take a snapshot. The query will check the index
    /// it existed at the moment the snapshot was taken.
    fn snapshot_nonfinalized_state(&self) -> &Self::Snapshot;

    /// Given inclusive start and end indexes, stream all blocks
    /// between the given indexes. Can be specified
    /// by hash or height.
    fn get_block_range<'snapshot, 'self_lt, 'future>(
        &'self_lt self,
        nonfinalized_snapshot: &'snapshot Self::Snapshot,
        start: Option<HashOrHeight>,
        end: Option<HashOrHeight>,
    ) -> Result<Option<impl Stream<Item = Result<Vec<u8>, Self::Error>> + 'future>, Self::Error>
    where
        'snapshot: 'future,
        'self_lt: 'future;
    /// Finds the newest ancestor of the given block on the main
    /// chain, or the block itself if it is on the main chain.
    fn find_fork_point(
        &self,
        snapshot: impl AsRef<Self::Snapshot>,
        block_hash: &types::Hash,
    ) -> Result<Option<(types::Hash, types::Height)>, Self::Error>;
    /// given a transaction id, returns the transaction
    fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: [u8; 32],
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>>;
    /// Given a transaction ID, returns all known
    fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: [u8; 32],
    ) -> Result<HashMap<types::Hash, Option<types::Height>>, Self::Error>;
}

/// A snapshot of the non-finalized state, for consistent queries
pub trait NonFinalizedSnapshot {
    /// Hash -> block
    fn get_chainblock_by_hash(&self, target_hash: &types::Hash) -> Option<&ChainBlock>;
    /// Height -> block
    fn get_chainblock_by_height(&self, target_height: &types::Height) -> Option<&ChainBlock>;
}

#[derive(Debug, thiserror::Error)]
#[error("{kind}: {message}")]
/// The set of errors that can occur during the public API calls
/// of a NodeBackedChainIndex
pub struct ChainIndexError {
    kind: ChainIndexErrorKind,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

#[derive(Debug)]
/// The high-level kinds of thing that can fail
pub enum ChainIndexErrorKind {
    /// Zaino is in some way nonfunctional
    InternalServerError,
    /// The given snapshot contains invalid data.
    InvalidSnapshot,
}

impl Display for ChainIndexErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChainIndexErrorKind::InternalServerError => "internal server error",
            ChainIndexErrorKind::InvalidSnapshot => "invalid snapshot",
        })
    }
}
