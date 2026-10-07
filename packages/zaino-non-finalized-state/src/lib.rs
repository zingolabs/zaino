//! Non-final window of the best chain, in memory: what a reorg replays from (no refetch)
//!
//! - Canonical only: a competing branch is learned by walking back from the verified tip's hash
//! - Floor = `highest tip seen before this advance − depth` (every legal fork point, and every
//!   block a sink reset can replay from: the trim runs at the start of the *next* advance)

use std::collections::VecDeque;
use std::sync::Arc;

use futures::TryStreamExt;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::{
    BlockFetchPool, ChainDataSource, GetBlockByHashError, GetBlockError, QueryError,
};

/// What [`ChainHead::advance`] changed; new blocks read back via [`ChainHead::best_chain_from`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    Unchanged,
    Extended,
    /// Heights `>= fork` replaced by the winning branch (none = a retreat onto `fork − 1`)
    Reorg {
        fork: Height,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AdvanceError {
    #[error("fetch by height: {0}")]
    FetchHeight(#[from] QueryError<GetBlockError>),
    #[error("fetch by hash: {0}")]
    FetchHash(#[from] QueryError<GetBlockByHashError>),
    /// Verified tip forks below the window = past the consensus reorg bound (final data is wrong)
    #[error("verified tip {tip:?} forks below the window floor {floor:?}")]
    BelowWindow { tip: BlockRef, floor: Height },
    /// Validator served block `hash` at a height its child contradicts (retryable: another tip)
    #[error("block {hash} at {got:?}, its child says {expected:?}")]
    Inconsistent { hash: BlockHash, got: Height, expected: Height },
}

pub struct ChainHead {
    window: VecDeque<Arc<Block>>,
    depth: ReorgDepth,
    highest: Height,
}

impl ChainHead {
    pub fn new(anchor: Arc<Block>, depth: ReorgDepth) -> Self {
        let highest = anchor.header().height;
        Self { window: VecDeque::from([anchor]), depth, highest }
    }

    pub fn tip(&self) -> BlockRef {
        let tip = &self.back().header();
        BlockRef { hash: tip.hash, height: tip.height }
    }

    pub fn floor(&self) -> Height {
        self.front().header().height
    }

    /// Floor the next [`advance`](Self::advance) enforces (its trim runs first): a tip below it
    /// = [`AdvanceError::BelowWindow`]
    pub fn next_floor(&self) -> Height {
        self.floor().max(self.highest.saturating_sub(self.depth.get()))
    }

    /// Canonical blocks `start` to the tip, both inclusive, ascending (`start` = tip + 1 → none)
    pub fn best_chain_from(&self, start: Height) -> impl Iterator<Item = &Arc<Block>> {
        let end = self.tip().height.checked_add(1).expect("tip below the height maximum");
        assert!(start <= end, "{start:?} past the window tip + 1 ({end:?})");
        self.window.range(self.offset(start)..)
    }

    /// Follows the verified tip; `tip` at most `depth` above ours (the producer bulk-fetches wider
    /// gaps, so the window never holds more than the non-final span)
    pub async fn advance<S>(
        &mut self,
        tip: BlockRef,
        pool: &BlockFetchPool<S>,
    ) -> Result<Advance, AdvanceError>
    where
        S: ChainDataSource,
    {
        let ours = self.tip();
        let reach = u32::from(ours.height) + self.depth.get();
        assert!(
            u32::from(tip.height) <= reach,
            "verified tip {tip:?} beyond the window of {ours:?}"
        );
        // what the last advance's caller has since published is now safe to drop
        self.trim();
        if tip == ours {
            return Ok(Advance::Unchanged);
        }

        let outcome = if self.holds(tip) {
            self.retreat(tip)
        } else {
            match self.extension(tip, pool).await? {
                Some(blocks) => {
                    self.window.extend(blocks);
                    Advance::Extended
                }
                None => self.walk_back(tip, pool).await?,
            }
        };
        self.highest = self.highest.max(tip.height);
        self.assert_linked();
        assert_eq!(self.tip(), tip, "advance ends on the verified tip");
        Ok(outcome)
    }

    /// Fast path: `ours + 1` to `tip`, both inclusive, by height, concurrently; `None` = does not
    /// link (a reorg)
    async fn extension<S>(
        &self,
        tip: BlockRef,
        pool: &BlockFetchPool<S>,
    ) -> Result<Option<Vec<Arc<Block>>>, AdvanceError>
    where
        S: ChainDataSource,
    {
        let ours = self.tip();
        let Some(from) = ours.height.checked_add(1).filter(|from| *from <= tip.height) else {
            return Ok(None);
        };
        let blocks: Vec<Arc<Block>> =
            pool.blocks(from, tip.height).map_ok(Arc::new).try_collect().await?;
        let linked = blocks.iter().try_fold(ours.hash, |parent, block| {
            (block.header().prev_hash == parent).then_some(block.header().hash)
        }) == Some(tip.hash);
        Ok(linked.then_some(blocks))
    }

    /// From `tip` back by `prev_hash` to the window block it links onto
    async fn walk_back<S>(
        &mut self,
        tip: BlockRef,
        pool: &BlockFetchPool<S>,
    ) -> Result<Advance, AdvanceError>
    where
        S: ChainDataSource,
    {
        let mut branch: Vec<Arc<Block>> = Vec::new();
        let mut want = BlockRef { hash: tip.hash, height: tip.height };
        let parent = loop {
            let block = Arc::new(pool.block_by_hash(want.hash).await?);
            if block.header().height != want.height {
                return Err(AdvanceError::Inconsistent {
                    hash: want.hash,
                    got: block.header().height,
                    expected: want.height,
                });
            }
            let parent = BlockRef {
                hash: block.header().prev_hash,
                height: block
                    .header()
                    .height
                    .checked_sub(1)
                    .ok_or(AdvanceError::BelowWindow { tip, floor: self.floor() })?,
            };
            want = parent;
            branch.push(block);
            if parent.height < self.floor() {
                return Err(AdvanceError::BelowWindow { tip, floor: self.floor() });
            }
            if self.holds(parent) {
                break parent;
            }
        };

        let fork = parent.height.checked_add(1).expect("parent below a fetched block");
        let extends_tip = parent == self.tip();
        self.window.truncate(self.offset(fork));
        self.window.extend(branch.into_iter().rev());
        Ok(if extends_tip { Advance::Extended } else { Advance::Reorg { fork } })
    }

    /// Held ancestor `tip` → window cut back onto it (every height above orphaned)
    fn retreat(&mut self, tip: BlockRef) -> Advance {
        let fork = tip.height.checked_add(1).expect("held tip below ours");
        self.window.truncate(self.offset(fork));
        Advance::Reorg { fork }
    }

    /// Canonical `block` held in the window
    fn holds(&self, block: BlockRef) -> bool {
        block.height >= self.floor()
            && self
                .window
                .get(self.offset(block.height))
                .is_some_and(|held| held.header().hash == block.hash)
    }

    /// Window index of `height` (`height >= floor`)
    fn offset(&self, height: Height) -> usize {
        let at = u32::from(height)
            .checked_sub(u32::from(self.floor()))
            .unwrap_or_else(|| panic!("{height:?} below the window floor {:?}", self.floor()));
        at as usize
    }

    /// Drops what the highest tip has buried below the non-final span (tip always kept)
    fn trim(&mut self) {
        let floor = u32::from(self.highest).saturating_sub(self.depth.get());
        while self.window.len() > 1 && u32::from(self.floor()) < floor {
            self.window.pop_front();
        }
    }

    /// Trimmed to one depth below the previous highest, then extended by at most one depth
    fn assert_linked(&self) {
        let (len, depth) = (self.window.len() as u64, u64::from(self.depth.get()));
        assert!(len <= 2 * depth + 1, "window {len} over 2 × depth {depth} + 1");
        for (below, above) in self.window.iter().zip(self.window.iter().skip(1)) {
            let (below, above) = (below.header(), above.header());
            assert_eq!(above.prev_hash, below.hash, "window unlinked at {:?}", above.height);
            let next = below.height.checked_add(1);
            assert_eq!(next, Some(above.height), "window gap above {:?}", below.height);
        }
    }

    fn front(&self) -> &Arc<Block> {
        self.window.front().expect("window never empty")
    }

    fn back(&self) -> &Arc<Block> {
        self.window.back().expect("window never empty")
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use std::collections::HashMap;

    use zaino_primitives::testing::Chain;
    use zaino_source::mock::MockChain;

    use super::*;

    /// Depth 3 over two validators, `a` on branch A (0 to 6, both inclusive), `b` forking after A4
    /// (B5 to B7, both inclusive), `c`
    /// forking after A2: extension (incl. a spread fetch that straddles branches), a reorg up, a
    /// reorg back down that keeps the floor, retreats onto and below it, and a fork below the window
    #[tokio::test]
    async fn follows_extensions_and_reorgs_and_refuses_a_fork_below_the_window() {
        let mut builder = Chain::new();
        let mut a = vec![builder.genesis()];
        for _ in 1..=6 {
            a.push(builder.mine(a[a.len() - 1].hash));
        }
        let mut b = vec![builder.mine(a[4].hash)];
        for _ in 6..=7 {
            b.push(builder.mine(b[b.len() - 1].hash));
        }
        let mut c = vec![builder.mine(a[2].hash)];
        for _ in 4..=6 {
            c.push(builder.mine(c[c.len() - 1].hash));
        }
        let names: HashMap<BlockHash, String> = [("a", &a, 0), ("b", &b, 5), ("c", &c, 3)]
            .into_iter()
            .flat_map(|(branch, blocks, from)| {
                (blocks.iter().zip(from..)).map(move |(at, h)| (at.hash, format!("{branch}{h}")))
            })
            .collect();
        let validator = |tip: &BlockRef| Arc::new(MockChain::serving(builder.path(tip.hash)));
        let pool = BlockFetchPool::new(
            vec![validator(&a[6]), validator(&b[2]), validator(&c[3])],
            NonZeroUsize::new(2).expect("nz"),
        );
        let height = |h: u32| Height::try_from(h).expect("h");
        let chain = |head: &ChainHead, start: u32| -> Vec<&str> {
            head.best_chain_from(height(start))
                .map(|block| names[&block.header().hash].as_str())
                .collect()
        };

        let anchor = Arc::new(builder.block(a[2].hash).clone());
        let mut head =
            ChainHead::new(anchor, ReorgDepth::new(std::num::NonZeroU32::new(3).expect("nz")));
        assert_eq!(head.advance(a[4], &pool).await.expect("a4"), Advance::Extended);
        assert_eq!(chain(&head, 2), ["a2", "a3", "a4"]);

        // height 5 spreads to `b` (B5), 6 to `a` (A6): unlinked → walk back from A6
        // floor = the previous highest (4) − depth, trimmed only as the next advance starts
        assert_eq!(head.advance(a[6], &pool).await.expect("a6"), Advance::Extended);
        assert_eq!((head.floor(), chain(&head, 3)), (height(2), vec!["a3", "a4", "a5", "a6"]));
        assert_eq!(head.next_floor(), height(3), "the next advance trims to highest − depth first");

        let reorg_at_5 = Advance::Reorg { fork: height(5) };
        assert_eq!(head.advance(b[2], &pool).await.expect("b7"), reorg_at_5);
        // A3 kept: a sink reset replays from the pre-advance tip's first non-final height
        let window = chain(&head, 3);
        assert_eq!((head.floor(), window), (height(3), vec!["a3", "a4", "b5", "b6", "b7"]));

        // lower winning tip: floor held at highest (7) − depth, never lowered
        assert_eq!(head.advance(a[5], &pool).await.expect("a5"), reorg_at_5);
        assert_eq!((head.floor(), chain(&head, 4)), (height(4), vec!["a4", "a5"]));

        // retreat onto a held ancestor = a reorg with no replacement block (C11)
        assert_eq!(head.advance(a[4], &pool).await.expect("a4"), reorg_at_5);
        assert_eq!((head.floor(), head.tip(), chain(&head, 4)), (height(4), a[4], vec!["a4"]));
        assert_eq!(head.advance(a[4], &pool).await.expect("a4 again"), Advance::Unchanged);
        let deeper = head.advance(a[3], &pool).await;
        let Err(AdvanceError::BelowWindow { floor, .. }) = &deeper else { panic!("{deeper:?}") };
        assert_eq!((*floor, head.tip()), (height(4), a[4]), "retreat below the floor");
        assert_eq!(head.advance(a[5], &pool).await.expect("a5 back"), Advance::Extended);

        let refused = head.advance(c[3], &pool).await;
        let Err(AdvanceError::BelowWindow { floor, .. }) = &refused else { panic!("{refused:?}") };
        assert_eq!(*floor, height(4));
        assert_eq!(head.tip(), a[5], "a refused advance leaves the window as it was");
    }
}
