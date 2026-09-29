//! Non-final window of the best chain, in memory: what a reorg replays from (no refetch)
//!
//! - Canonical only: a competing branch is learned by walking back from the quorum tip's hash
//! - Floor = `highest tip seen before this advance − depth` (every legal fork point, and every
//!   block a sink reset can replay from: the trim runs at the start of the *next* advance)

use std::collections::VecDeque;
use std::sync::Arc;

use futures::TryStreamExt;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::{
    BlockFetchPool, GetBlock, GetBlockByHash, GetBlockByHashError, GetBlockError, QueryError,
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
    /// Quorum tip forks below the window = past the consensus reorg bound (final data is wrong)
    #[error("quorum tip {tip:?} forks below the window floor {floor:?}")]
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

    /// Canonical blocks `from..=tip`, ascending (`from = tip + 1` = none)
    pub fn best_chain_from(&self, from: Height) -> impl Iterator<Item = &Arc<Block>> {
        let end = self.tip().height.checked_add(1).expect("tip below the height maximum");
        assert!(from <= end, "{from:?} past the window tip + 1 ({end:?})");
        self.window.range(self.offset(from)..)
    }

    /// Follows the quorum tip; `tip` at most `depth` above ours (the producer bulk-fetches wider
    /// gaps, so the window never holds more than the non-final span)
    pub async fn advance<S>(
        &mut self,
        tip: BlockRef,
        pool: &BlockFetchPool<S>,
    ) -> Result<Advance, AdvanceError>
    where
        S: GetBlock + GetBlockByHash + Send + Sync + 'static,
    {
        let ours = self.tip();
        let reach = u32::from(ours.height) + self.depth.get();
        assert!(u32::from(tip.height) <= reach, "quorum tip {tip:?} beyond the window of {ours:?}");
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
        assert_eq!(self.tip(), tip, "advance ends on the quorum tip");
        Ok(outcome)
    }

    /// Fast path: `ours + 1 ..= tip` by height, concurrently; `None` = does not link (a reorg)
    async fn extension<S>(
        &self,
        tip: BlockRef,
        pool: &BlockFetchPool<S>,
    ) -> Result<Option<Vec<Arc<Block>>>, AdvanceError>
    where
        S: GetBlock + GetBlockByHash + Send + Sync + 'static,
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
        S: GetBlock + GetBlockByHash + Send + Sync + 'static,
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

    use zaino_primitives::types::{BlockHeader, Transaction};
    use zaino_source::mock::MockChain;
    use zaino_source::FetchRoute;

    use super::*;

    /// Depth 3 over two validators, `a` on branch A (0..=6), `b` forking after A4 (B5..=B7), `c`
    /// forking after A2: extension (incl. a spread fetch that straddles branches), a reorg up, a
    /// reorg back down that keeps the floor, retreats onto and below it, and a fork below the window
    #[tokio::test]
    async fn follows_extensions_and_reorgs_and_refuses_a_fork_below_the_window() {
        let block = |height: u32, byte: u8, parent: u8| {
            Arc::new(Block::new(
                BlockHeader::for_tests(height, [byte; 32], [parent; 32], 0),
                vec![Transaction {
                    txid: [byte; 32].into(),
                    transparent: Default::default(),
                    sprout: Default::default(),
                    sapling: Default::default(),
                    orchard: Default::default(),
                    ironwood: Default::default(),
                }],
            ))
        };
        let a: Vec<_> = (0..=6).map(|h| block(h, 0x10 + h as u8, 0x0f + h as u8)).collect();
        let b = [block(5, 0x25, 0x14), block(6, 0x26, 0x25), block(7, 0x27, 0x26)];
        let c = [
            block(3, 0x33, 0x12),
            block(4, 0x34, 0x33),
            block(5, 0x35, 0x34),
            block(6, 0x36, 0x35),
        ];
        let validator = |blocks: Vec<&Arc<Block>>| {
            Arc::new(
                blocks
                    .into_iter()
                    .fold(MockChain::new(), |chain, block| chain.with_block(Block::clone(block))),
            )
        };
        let pool = BlockFetchPool::new(
            vec![
                validator(a.iter().collect()),
                validator(a[..=4].iter().chain(&b).collect()),
                validator(a[..=2].iter().chain(&c).collect()),
            ],
            FetchRoute::Spread,
            NonZeroUsize::new(2).expect("nz"),
        );
        let at = |block: &Arc<Block>| BlockRef {
            hash: block.header().hash,
            height: block.header().height,
        };
        let height = |h: u32| Height::try_from(h).expect("h");
        let chain = |head: &ChainHead, from: u32| -> Vec<u8> {
            head.best_chain_from(height(from))
                .map(|block| <[u8; 32]>::from(block.header().hash)[0])
                .collect()
        };

        let mut head = ChainHead::new(
            Arc::clone(&a[2]),
            ReorgDepth::new(std::num::NonZeroU32::new(3).expect("nz")),
        );
        assert_eq!(head.advance(at(&a[4]), &pool).await.expect("a4"), Advance::Extended);
        assert_eq!(chain(&head, 2), [0x12, 0x13, 0x14]);

        // height 5 spreads to `b` (B5), 6 to `a` (A6): unlinked → walk back from A6
        // floor = the previous highest (4) − depth, trimmed only as the next advance starts
        assert_eq!(head.advance(at(&a[6]), &pool).await.expect("a6"), Advance::Extended);
        assert_eq!((head.floor(), chain(&head, 3)), (height(2), vec![0x13, 0x14, 0x15, 0x16]));

        let reorg_at_5 = Advance::Reorg { fork: height(5) };
        assert_eq!(head.advance(at(&b[2]), &pool).await.expect("b7"), reorg_at_5);
        // A3 kept: a sink reset replays from the pre-advance tip's first non-final height
        let window = chain(&head, 3);
        assert_eq!((head.floor(), window), (height(3), vec![0x13, 0x14, 0x25, 0x26, 0x27]));

        // lower winning tip: floor held at highest (7) − depth, never lowered
        assert_eq!(head.advance(at(&a[5]), &pool).await.expect("a5"), reorg_at_5);
        assert_eq!((head.floor(), chain(&head, 4)), (height(4), vec![0x14, 0x15]));

        // retreat onto a held ancestor = a reorg with no replacement block (C11)
        assert_eq!(head.advance(at(&a[4]), &pool).await.expect("a4"), reorg_at_5);
        assert_eq!((head.floor(), head.tip(), chain(&head, 4)), (height(4), at(&a[4]), vec![0x14]));
        assert_eq!(head.advance(at(&a[4]), &pool).await.expect("a4 again"), Advance::Unchanged);
        let deeper = head.advance(at(&a[3]), &pool).await;
        let Err(AdvanceError::BelowWindow { floor, .. }) = &deeper else { panic!("{deeper:?}") };
        assert_eq!((*floor, head.tip()), (height(4), at(&a[4])), "retreat below the floor");
        assert_eq!(head.advance(at(&a[5]), &pool).await.expect("a5 back"), Advance::Extended);

        let refused = head.advance(at(&c[3]), &pool).await;
        let Err(AdvanceError::BelowWindow { floor, .. }) = &refused else { panic!("{refused:?}") };
        assert_eq!(*floor, height(4));
        assert_eq!(head.tip(), at(&a[5]), "a refused advance leaves the window as it was");
    }
}
