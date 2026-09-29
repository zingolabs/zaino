//! Non-finalized map + committed segments, pinned together once per request

use std::sync::Arc;

use imbl::HashMap;
use zaino_persistence::lsm;
use zaino_primitives::types::Height;

use crate::{
    by_hash::{HashKey, HashRow},
    HASH,
};

#[derive(Clone)]
pub struct ReadView {
    non_finalized: HashMap<[u8; HASH], Height>,
    segments: Arc<lsm::Snapshot<HashKey>>,
}

impl ReadView {
    pub(crate) fn new(
        non_finalized: HashMap<[u8; HASH], Height>,
        segments: Arc<lsm::Snapshot<HashKey>>,
    ) -> Self {
        Self { non_finalized, segments }
    }

    pub(crate) fn height_of_hash(&self, hash: &[u8; HASH]) -> Option<Height> {
        if let Some(height) = self.non_finalized.get(hash) {
            return Some(*height);
        }
        let row = self.segments.get::<HashRow>(&HashKey(*hash))?;
        Some(Height::try_from(row.height).expect("committed heights were valid when written"))
    }
}

impl std::fmt::Debug for ReadView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadView")
            .field("non_finalized", &self.non_finalized.len())
            .finish_non_exhaustive()
    }
}
