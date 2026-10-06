//! One endpoint's best chain: its tip + `depth` ancestors, hash-linked (its vote until phase 5, Status)
//!
//! - Built by [`Walk`]: down from a reported tip by `prev_hash` until it joins the held chain or
//!   reaches the floor (steady state = one `getblockheader` per new block)

use imbl::Vector;
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::BlockLink;

/// `hashes[i]` = the endpoint's block at `floor + i`; never empty (last = tip)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EndpointChain {
    floor: Height,
    hashes: Vector<BlockHash>,
}

impl EndpointChain {
    pub(crate) fn tip(&self) -> BlockRef {
        let above = u32::try_from(self.hashes.len() - 1).expect("window of at most depth + 1");
        BlockRef {
            hash: *self.hashes.last().expect("chain never empty"),
            height: self.floor.checked_add(above).expect("tip at a held height"),
        }
    }

    pub(crate) fn floor(&self) -> Height {
        self.floor
    }

    pub(crate) fn hash_at(&self, height: Height) -> Option<BlockHash> {
        let offset = u32::from(height).checked_sub(u32::from(self.floor))?;
        self.hashes.get(offset as usize).copied()
    }

    pub(crate) fn holds(&self, block: BlockRef) -> bool {
        self.hash_at(block.height) == Some(block.hash)
    }

    /// Every block held, floor first
    pub(crate) fn blocks(&self) -> impl Iterator<Item = BlockRef> + '_ {
        self.floor
            .up_to(self.tip().height)
            .zip(self.hashes.iter())
            .map(|(height, hash)| BlockRef { hash: *hash, height })
    }

    /// Bare tip, no ancestry (a walk that never had to descend)
    fn single(tip: BlockRef) -> Self {
        Self { floor: tip.height, hashes: Vector::unit(tip.hash) }
    }

    #[cfg(test)]
    pub(crate) fn of(floor: Height, hashes: impl IntoIterator<Item = BlockHash>) -> Self {
        let hashes: Vector<BlockHash> = hashes.into_iter().collect();
        assert!(!hashes.is_empty(), "a chain holds its tip");
        Self { floor, hashes }
    }
}

/// Endpoint's chain moved under the walk (reorged since it reported its tip)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Raced;

/// Descent from a reported tip: `want` = the endpoint's hash at `at`
#[derive(Debug)]
pub(crate) struct Walk {
    floor: Height,
    at: Height,
    want: BlockHash,
    /// Hashes at `at + 1 ..= tip`, highest first
    above: Vec<BlockHash>,
}

impl Walk {
    pub(crate) fn new(tip: BlockRef, depth: ReorgDepth) -> Self {
        Self {
            floor: tip.height.saturating_sub(depth.get()),
            at: tip.height,
            want: tip.hash,
            above: Vec::new(),
        }
    }

    /// Height whose link [`descend`](Self::descend) takes next
    pub(crate) fn next(&self) -> Height {
        self.at
    }

    /// Lowest height a link is still needed at (`floor`'s hash = its child's `prev_hash`)
    pub(crate) fn lowest(&self) -> Height {
        self.floor.next()
    }

    /// `link` = the endpoint's block at [`next`](Self::next); only after `finish` = `None`
    pub(crate) fn descend(&mut self, link: BlockLink) -> Result<(), Raced> {
        if link.hash != self.want {
            return Err(Raced);
        }
        self.above.push(self.want);
        self.at = self.at.checked_sub(1).expect("unfinished walk sits above its floor");
        self.want = link.prev_hash;
        Ok(())
    }

    /// `Some` once joined onto `held` or down to the floor; `None` = another link needed
    ///
    /// - Join only onto a held chain reaching this walk's floor (a retreat lowers the floor)
    pub(crate) fn finish(&self, held: Option<&EndpointChain>) -> Option<EndpointChain> {
        let joined = held
            .filter(|held| held.floor <= self.floor && held.hash_at(self.at) == Some(self.want));
        let base = match joined {
            Some(held) => EndpointChain {
                floor: held.floor,
                hashes: held.hashes.take((u32::from(self.at) - u32::from(held.floor)) as usize + 1),
            },
            None if self.at <= self.floor || self.at == Height::GENESIS => {
                EndpointChain::single(BlockRef { hash: self.want, height: self.at })
            }
            None => return None,
        };
        let mut chain = base;
        chain.hashes.extend(self.above.iter().rev().copied());
        let below = u32::from(self.floor).saturating_sub(u32::from(chain.floor));
        if below > 0 {
            chain.hashes = chain.hashes.skip(below as usize);
            chain.floor = self.floor;
        }
        Some(chain)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    /// Trunk hash at `h` = `[h; 32]`, fork `f` = `[f | h; 32]` (distinct per branch and height)
    #[test]
    fn walks_join_the_held_chain_or_reach_the_floor_and_refuse_a_moved_chain() {
        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("nz"));
        let height = |h: u32| Height::try_from(h).expect("h");
        let hash = |branch: u8, h: u32| BlockHash::from([branch | h as u8; 32]);
        let block = |branch: u8, h: u32| BlockRef { hash: hash(branch, h), height: height(h) };
        let link = |branch: u8, h: u32, parent: u8| BlockLink {
            hash: hash(branch, h),
            prev_hash: hash(parent, h - 1),
        };
        let walk = |tip: BlockRef, held: Option<&EndpointChain>, links: &[BlockLink]| {
            let mut walk = Walk::new(tip, depth);
            let mut fetched = Vec::new();
            for link in links {
                if let Some(chain) = walk.finish(held) {
                    return (Ok(chain), fetched);
                }
                fetched.push(walk.next());
                if let Err(raced) = walk.descend(*link) {
                    return (Err(raced), fetched);
                }
            }
            (Ok(walk.finish(held).expect("test supplies every link the walk needs")), fetched)
        };
        let blocks = |chain: &EndpointChain| chain.blocks().collect::<Vec<_>>();

        // first build: floor = tip − depth, every hash from a child's `prev_hash`
        let trunk: Vec<BlockLink> = (7..=10).rev().map(|h| link(0, h, 0)).collect();
        let (built, fetched) = walk(block(0, 10), None, &trunk);
        let built = built.expect("floor reached");
        assert_eq!(fetched, [10, 9, 8].map(height));
        assert_eq!(blocks(&built), [7, 8, 9, 10].map(|h| block(0, h)));
        assert_eq!((built.floor(), built.tip()), (height(7), block(0, 10)));

        // steady state: one link joins, the window slides (floor 7 → 8)
        let (next, fetched) = walk(block(0, 11), Some(&built), &[link(0, 11, 0)]);
        let next = next.expect("joined at 10");
        assert_eq!(
            (fetched, blocks(&next)),
            (vec![height(11)], [8, 9, 10, 11].map(|h| block(0, h)).to_vec())
        );
        let (same, fetched) = walk(block(0, 11), Some(&next), &[]);
        assert_eq!((same, fetched), (Ok(next.clone()), vec![]), "unchanged tip = no fetch");

        // reorg: fork 0x40 from 9 up; the walk descends past the held 10 and 11 to join at 9
        let fork = [link(0x40, 12, 0x40), link(0x40, 11, 0x40), link(0x40, 10, 0)];
        let (reorged, fetched) = walk(block(0x40, 12), Some(&next), &fork);
        let expected = [block(0, 9), block(0x40, 10), block(0x40, 11), block(0x40, 12)];
        assert_eq!(
            (fetched, blocks(&reorged.expect("joined at 9"))),
            ([12, 11, 10].map(height).to_vec(), expected.to_vec())
        );

        // retreat onto a held ancestor: the floor drops (8 → 6), so the walk re-reads down to it
        let below_held: Vec<BlockLink> = (7..=9).rev().map(|h| link(0, h, 0)).collect();
        let (retreat, fetched) = walk(block(0, 9), Some(&next), &below_held);
        assert_eq!(
            (fetched, blocks(&retreat.expect("floor reached"))),
            ([9, 8, 7].map(height).to_vec(), [6, 7, 8, 9].map(|h| block(0, h)).to_vec())
        );

        // endpoint's chain moved mid-walk: a link that is not the wanted hash = raced
        let (at_tip, _) = walk(block(0, 11), None, &[link(0x40, 11, 0x40)]);
        assert_eq!(at_tip, Err(Raced), "tip reported, then replaced before its header was read");
        let (below, _) =
            walk(block(0x40, 12), Some(&next), &[link(0x40, 12, 0x40), link(0, 11, 0)]);
        assert_eq!(below, Err(Raced), "parent replaced mid-descent");

        // near genesis: the window stops at 0
        let (genesis, fetched) = walk(block(0, 1), None, &[link(0, 1, 0)]);
        assert_eq!(
            (fetched, blocks(&genesis.expect("genesis"))),
            (vec![height(1)], [0, 1].map(|h| block(0, h)).to_vec())
        );
    }
}
