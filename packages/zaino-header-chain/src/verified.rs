//! [`VerifiedChain`]: the one value everything downstream reads (`verified-chain.md` §3)
//!
//! - clone = refcounts (`imbl` path, store view at the final tip): a holder's answers never change

use zaino_primitives::types::{BlockHash, BlockRef, Height};

use crate::chain::BestTip;
use crate::store::{HeaderView, Record};

/// Above the final tip: `above[i]` = best branch at `final + 1 + i`; at or below: the store's view
#[derive(Debug, Clone)]
pub struct VerifiedChain {
    best: BestTip,
    final_tip: Option<BlockRef>,
    above: imbl::Vector<Record>,
    finals: HeaderView,
}

/// zcashd `CChain::GetLocator`: the step doubles once the locator holds more than this many
const CONSECUTIVE: usize = 10;

impl VerifiedChain {
    pub(crate) fn new(
        best: BestTip,
        final_tip: Option<BlockRef>,
        above: imbl::Vector<Record>,
        finals: HeaderView,
    ) -> Self {
        assert_eq!(finals.tip(), final_tip, "H5: the store view sits at the final tip");
        Self { best, final_tip, above, finals }
    }

    pub fn best(&self) -> BlockRef {
        self.best.block
    }

    /// Never moves back (`None` = nothing final yet)
    pub fn final_tip(&self) -> Option<BlockRef> {
        self.final_tip
    }

    /// Best-chain hash at `height` (`None` = above the best tip)
    pub fn hash_at(&self, height: Height) -> Option<BlockHash> {
        self.header_at(height).map(|record| record.hash)
    }

    /// Best-chain header at `height`: hash, merkle root, time, nBits, cumulative work
    pub fn header_at(&self, height: Height) -> Option<Record> {
        if height > self.best.block.height {
            return None;
        }
        let base = self.final_tip.map_or(Height::GENESIS, |tip| tip.height.next());
        match u32::from(height).checked_sub(u32::from(base)) {
            Some(above) => self.above.get(above as usize).copied(),
            None => self.finals.record(height),
        }
    }

    /// zcashd's `getheaders` locator: the best tip, ten consecutive ancestors, then doubling
    /// steps, ending at the final tip (genesis while nothing is final)
    pub fn locator(&self) -> Vec<BlockHash> {
        let floor = self.final_tip.map_or(0, |tip| u32::from(tip.height));
        let mut at = u32::from(self.best.block.height);
        let mut step = 1;
        let mut locator = Vec::new();
        loop {
            let height = Height::try_from(at).expect("between the floor and the best tip");
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

    /// `path` (a `testing::Chain`'s, genesis first) verified under regtest rules, nothing final
    #[cfg(feature = "testing")]
    pub fn regtest(path: &[zaino_primitives::types::Block]) -> Self {
        let genesis = path.first().expect("a path holds genesis").header().hash;
        let depth = zaino_primitives::types::ReorgDepth::CONSENSUS;
        let mut chain = crate::HeaderChain::regtest_in_memory(genesis, depth);
        chain.insert_blocks(path).expect("a testing::Chain path verifies");
        chain.verified().expect("genesis verified")
    }
}
