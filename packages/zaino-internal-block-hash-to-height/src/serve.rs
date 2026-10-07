//! Hash → height for the serving path
//!
//! - answer = a height on this index's chain
//! - serving index confirms it holds `hash` there (independent publications: a reorg can land
//!   between the reads)

use zaino_persistence::{LayeredView, MapRead};
use zaino_primitives::types::{BlockHash, Height};
use zaino_sync::Served;

use crate::{BlockHashReader, HASH};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    /// No progress carried (a partial locator = "not indexed yet" read as "no such block")
    #[error("the block-hash index is still syncing")]
    Syncing,

    #[error("block hash is not in the index")]
    HashNotFound,
}

#[derive(Debug, Clone)]
pub struct BlockHashService<V> {
    served: Served<BlockHashReader<LayeredView<V>>>,
}

impl<V: MapRead> BlockHashService<V> {
    /// Unsynced → every call [`ServeError::Syncing`]
    pub fn new(served: Served<BlockHashReader<LayeredView<V>>>) -> Self {
        Self { served }
    }

    pub fn locate(&self, hash: &[u8; HASH]) -> Result<Height, ServeError> {
        let reader = self.served.pin().ok_or(ServeError::Syncing)?;
        reader.height_of(&BlockHash::from(*hash)).ok_or(ServeError::HashNotFound)
    }
}
