//! [`TreeStateReader`] + its primitive reads (tip, sizes, frontier, records) over any view of the
//! tables (serving's and a fold's parent); RPC answers = `serve.rs`

use incrementalmerkletree::{
    frontier::{Frontier, NonEmptyFrontier},
    Address, Hashable, Position, Source,
};
use zaino_persistence::{OverlayView, SequenceRead, SequenceTable, View};
use zaino_primitives::types::TreeSizes;
use zcash_primitives::merkle_tree::HashSer;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::ShieldedPool;

use crate::{
    heights::{self, TreeStateHeight},
    nodes::{self, slot, MERKLE_DEPTH},
    tables::{level_table, HEIGHTS},
};

/// One state of the tables, never moving while held (one pin per request, one parent per fold)
#[derive(Clone)]
pub struct TreeStateReader<V> {
    view: V,
}

impl<V: View> std::fmt::Debug for TreeStateReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeStateReader").field("tip", &self.view.tip()).finish_non_exhaustive()
    }
}

impl<V: View> TreeStateReader<V> {
    /// `view` of a store opened with [`schema`](crate::schema)
    pub fn new(view: V) -> Self {
        Self { view }
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }

    /// Last height held, inclusive (`None` = nothing held)
    pub(crate) fn tip(&self) -> Option<BlockHeight> {
        self.view.tip().map(|tip| tip.height)
    }
}

/// NFS publication's seam: its layer above the durable files
impl<V: View> TreeStateReader<OverlayView<V>> {
    /// `at` in the layer above the durable files: among the ~1000 heights every synced wallet
    /// asks about (bounded: a per-publication memo keyed on these stays small)
    pub fn is_above_durable(&self, at: BlockHeight) -> bool {
        let durable = self.view.durable().tip().map(|tip| tip.height);
        Some(at) <= self.tip() && Some(at) > durable
    }
}

impl<V: SequenceRead> TreeStateReader<V> {
    /// Tree sizes after the tip ([`TreeSizes::ZERO`] = nothing held)
    pub(crate) fn tip_sizes(&self) -> TreeSizes {
        let Some(tip) = self.tip() else {
            return TreeSizes::ZERO;
        };
        self.height_record(tip).expect("a view holds its tip's record").sizes
    }

    /// `pool`'s frontier at tree size `size` (`None` = a node missing or non-canonical: corrupt)
    ///
    /// - no hashing: every ommer = a retained left sibling, leaf = `l00[size - 1]`
    pub(crate) fn frontier<H: Hashable + HashSer + Clone>(
        &self,
        pool: ShieldedPool,
        size: u64,
    ) -> Option<Frontier<H, MERKLE_DEPTH>> {
        let Some(last) = size.checked_sub(1) else {
            return Some(Frontier::empty());
        };
        let position = Position::from(last);
        let node = |addr| {
            let (level, index) = slot(addr)?;
            let record = self.view.sequence(level_table(pool, level)).record(index)?;
            nodes::decode::<H>(record[..].try_into().expect("NODE bytes"))
        };

        // `Source::Past(i)` indexes `ommers[i]` in `witness_addrs`' ascending-level order (that
        // order = the contract)
        let ommers = position
            .witness_addrs(position.root_level())
            .filter(|(_, source)| matches!(source, Source::Past(_)))
            .map(|(addr, _)| node(addr))
            .collect::<Option<Vec<H>>>()?;
        let leaf = node(Address::from(position))?;

        NonEmptyFrontier::from_parts(position, leaf, ommers).and_then(Frontier::try_from).ok()
    }

    /// Records in `table` (= the slot of its next append)
    pub(crate) fn len(&self, table: SequenceTable) -> u64 {
        self.view.sequence(table).len()
    }

    /// `None` above the tip
    pub(crate) fn height_record(&self, at: BlockHeight) -> Option<TreeStateHeight> {
        let bytes = self.view.sequence(HEIGHTS).record(u64::from(at))?;
        Some(heights::decode(bytes[..].try_into().expect("HEIGHTS width = RECORD bytes")))
    }
}
