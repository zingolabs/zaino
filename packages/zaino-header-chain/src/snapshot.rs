//! [`HeaderChainSnapshot`]: the one value everything downstream reads
//!
//! - clone = refcounts (`imbl`): a holder's answers never change
//! - at or below the finalized tip = finalized by definition (a trusted validator held it)
//! - only the newest `2 · depth` finalized headers answer by height

use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

use crate::chain::{BestTip, Node, Record};

/// - `above[i]` = best branch at `finalized + 1 + i`
/// - `finalized_headers` = newest finalized headers, oldest first, the finalized tip last
/// - `nodes` = the header tree above the finalized tip, every branch
#[derive(Debug, Clone)]
pub struct HeaderChainSnapshot {
    best: BestTip,
    above: imbl::Vector<Record>,
    finalized_headers: imbl::Vector<(BlockHeight, Record)>,
    nodes: imbl::HashMap<BlockHash, Node>,
}

/// zcashd `CChain::GetLocator`: the step doubles once the locator holds more than this many
const CONSECUTIVE: usize = 10;

impl HeaderChainSnapshot {
    pub(crate) fn new(
        best: BestTip,
        above: imbl::Vector<Record>,
        finalized_headers: imbl::Vector<(BlockHeight, Record)>,
        nodes: imbl::HashMap<BlockHash, Node>,
    ) -> Self {
        assert!(
            !finalized_headers.is_empty(),
            "H5: a published chain has a finalized tip (its anchor)"
        );
        Self { best, above, finalized_headers, nodes }
    }

    /// On the best chain (finalized included), or a side block held above the finalized tip
    pub fn holds(&self, at: BlockRef) -> bool {
        let side = self.nodes.get(&at.hash).is_some_and(|node| node.height == at.height);
        side || self.on_best(at)
    }

    /// On the best chain: a held header's hash, or at or below the finalized tip past the headers
    /// kept (finalized by definition)
    pub fn on_best(&self, at: BlockRef) -> bool {
        match self.header_at(at.height) {
            Some(record) => record.hash == at.hash,
            None => at.height <= self.finalized_tip().height,
        }
    }

    pub fn best(&self) -> BlockRef {
        self.best.block
    }

    /// A trusted validator had the best-chain block at `height` (or it is finalized: settled)
    pub fn vouched(&self, height: BlockHeight) -> bool {
        let node = self.hash_at(height).and_then(|hash| self.nodes.get(&hash));
        node.map_or(height <= self.finalized_tip().height, |node| node.witnesses.vouched())
    }

    /// Never moves back but on a re-anchor (a trusted validator far ahead)
    pub fn finalized_tip(&self) -> BlockRef {
        let (height, record) =
            self.finalized_headers.back().expect("a published chain has a finalized tip");
        BlockRef { hash: record.hash, height: *height }
    }

    /// Best-chain hash at `height` (`None` = above the best tip, or finalized below the headers
    /// kept)
    pub fn hash_at(&self, height: BlockHeight) -> Option<BlockHash> {
        self.header_at(height).map(|record| record.hash)
    }

    /// Best-chain header at `height`: hash, time
    pub fn header_at(&self, height: BlockHeight) -> Option<Record> {
        if height > self.best.block.height {
            return None;
        }
        let (oldest, _) =
            self.finalized_headers.front().expect("a published chain has a finalized tip");
        let at = u32::from(height).checked_sub(u32::from(*oldest))? as usize;
        match at.checked_sub(self.finalized_headers.len()) {
            Some(above) => self.above.get(above).copied(),
            None => self.finalized_headers.get(at).map(|(_, record)| *record),
        }
    }

    /// zcashd's `getheaders` locator: the best tip, ten consecutive ancestors, then doubling
    /// steps, ending at the finalized tip
    pub fn locator(&self) -> Vec<BlockHash> {
        let floor = u32::from(self.finalized_tip().height);
        let mut at = u32::from(self.best.block.height);
        let mut step = 1;
        let mut locator = Vec::new();
        loop {
            let height = BlockHeight::from(at);
            locator.push(self.hash_at(height).expect("every height up to the best is held"));
            if at == floor {
                return locator;
            }
            at = at.saturating_sub(step).max(floor);
            if locator.len() > CONSECUTIVE {
                step = step.saturating_mul(2);
            }
        }
    }
}
