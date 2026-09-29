//! Hash → height for the serving path
//!
//! - answer = a height on this index's chain
//! - serving index confirms it holds `hash` there (independent publications: a reorg can land
//!   between the reads)

use zaino_primitives::types::Height;
use zaino_sync::Served;

use crate::{ReadView, HASH};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    /// No progress carried (a partial locator = "not indexed yet" read as "no such block")
    #[error("the block-hash index is still syncing")]
    Syncing,

    #[error("block hash is not in the index")]
    HashNotFound,
}

#[derive(Debug, Clone)]
pub struct BlockHashService {
    served: Served<ReadView>,
}

impl BlockHashService {
    /// Unsynced → every call [`ServeError::Syncing`]
    pub fn new(served: Served<ReadView>) -> Self {
        Self { served }
    }

    pub fn locate(&self, hash: &[u8; HASH]) -> Result<Height, ServeError> {
        let view = self.served.pin().ok_or(ServeError::Syncing)?;
        view.height_of_hash(hash).ok_or(ServeError::HashNotFound)
    }
}
