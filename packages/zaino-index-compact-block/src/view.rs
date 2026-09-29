//! One pinned view of every tier a read resolves against
//!
//! - loaded **once** per request or stream (one atomic load, not one per refill)
//! - nonfinalised/files seam = a field of the view: "which tier owns `h`" has one answer for the
//!   request's life, a commit landing mid-stream cannot move it
//! - long-range safety from pinned bytes, not from durable records being append-only

use std::sync::Arc;

use bytes::Bytes;
use zaino_primitives::types::{Extent, Height};

use crate::{project::record_hash, NonFinalizedState, Pools, Snapshot, HASH};

/// Nonfinalised + durable, one publication
///
/// - clone = a pointer bump + an `imbl` structural share (republished on every applied block)
/// - `tip` resolved at publication (the writer's thread): `GetLatestBlock` then reads no page;
///   `(height, None)` = a tip record that will not walk
#[derive(Clone)]
pub struct ReadView {
    non_finalized: NonFinalizedState,
    durable: Arc<Snapshot>,
    tip: Option<(Height, Option<[u8; HASH]>)>,
}

impl ReadView {
    pub(crate) fn new(non_finalized: NonFinalizedState, durable: Arc<Snapshot>) -> Self {
        if let Some(root) = non_finalized.root_height() {
            assert_eq!(root, durable.end.next(), "nonfinalised must start where the files end");
        }
        let tip = match non_finalized.tip_id() {
            Some((height, hash)) => Some((height, Some(hash))),
            None => durable.end.last().map(|height| {
                (height, durable.block(height).and_then(|record| record_hash(&record)))
            }),
        };
        Self { non_finalized, durable, tip }
    }

    /// Tip height + its record's hash, resolved at publication
    pub(crate) fn tip_id(&self) -> Option<(Height, Option<[u8; HASH]>)> {
        self.tip
    }

    /// Heights the files answer (the seam)
    pub(crate) fn finalized_through(&self) -> Extent {
        self.durable.end
    }

    /// Heights any tier answers
    pub(crate) fn tip(&self) -> Extent {
        self.non_finalized.tip_height().map_or(self.finalized_through(), Extent::through)
    }

    /// Anything applied but not yet durable
    #[cfg(test)]
    pub(crate) fn has_non_finalized(&self) -> bool {
        !self.non_finalized.is_empty()
    }

    /// Framed, wire-ready; nonfinalised first (holds the newest blocks)
    pub(crate) fn block(&self, height: Height) -> Option<Bytes> {
        self.non_finalized.block(height).or_else(|| self.durable.block(height))
    }

    /// Nonfinalised tier only: RAM, no page touched (`None` = not there, maybe in the files)
    pub(crate) fn resident_block(&self, height: Height) -> Option<Bytes> {
        self.non_finalized.block(height)
    }

    /// Nonfinalised record projected to `pools` (the default shape precomputed at apply)
    pub(crate) fn resident_projected(&self, height: Height, pools: Pools) -> Option<Bytes> {
        self.non_finalized.projected(height, pools)
    }

    /// Record-aligned prefix of `from ..= to` from the files, plus the heights it reaches
    pub(crate) fn span_from(
        &self,
        from: Height,
        to: Height,
        budget: usize,
    ) -> Option<(Bytes, Extent)> {
        self.durable.span_from(from, to, budget)
    }
}

impl std::fmt::Debug for ReadView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadView")
            .field("finalized_through", &self.finalized_through())
            .field("tip", &self.tip())
            .finish_non_exhaustive()
    }
}
