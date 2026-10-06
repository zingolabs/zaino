//! Majority of the **configured** set.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use zaino_primitives::types::BlockRef;

use crate::chain::EndpointChain;
use crate::endpoints::{EndpointIndex, EndpointSet};
use crate::error::BelowQuorum;

/// `⌊N/2⌋ + 1` over the configured endpoints.
///
/// Configured, not responding: majority-of-responding is trivially subvertible by DoSing the
/// honest nodes. N=1 gives threshold 1 — quorum trivially met. Until phase 5
/// (`docs/design/chainview.md` Status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quorum {
    configured: NonZeroUsize,
    threshold: NonZeroUsize,
}

impl Quorum {
    pub(crate) fn over(configured: NonZeroUsize) -> Self {
        let threshold = configured.get() / 2 + 1;
        Self {
            configured,
            threshold: NonZeroUsize::new(threshold).expect("n/2 + 1 >= 1 for any n"),
        }
    }

    pub fn configured(&self) -> usize {
        self.configured.get()
    }

    pub fn threshold(&self) -> usize {
        self.threshold.get()
    }

    pub(crate) fn met_by(&self, endpoints: EndpointSet) -> bool {
        endpoints.count() >= self.threshold()
    }

    pub(crate) fn shortfall(&self, endpoints: EndpointSet) -> BelowQuorum {
        BelowQuorum {
            agreeing: endpoints.count(),
            threshold: self.threshold(),
            configured: self.configured(),
        }
    }
}

/// A block ≥threshold endpoints' best chains hold, **by hash**.
///
/// - Highest such block, never the maximum height (one node claiming 999,999 moves nothing)
/// - `agreed_by` = every voter holding it: its own tip, or an ancestor of it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuorumTip {
    pub block: BlockRef,
    pub agreed_by: EndpointSet,
}

/// One count over the voters' chains
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) tip: Option<QuorumTip>,
    /// Largest group holding one common block (below quorum: the shortfall's numerator)
    pub(crate) agreeing: EndpointSet,
}

/// Each voter's vote = every block its chain holds (tip + ancestors in its window)
///
/// - Highest met block unique (two blocks at one height can't both reach a majority)
/// - `O(voters × depth)` (no height scan: a far-off claimed tip costs one window, not a range)
pub(crate) fn tally<'a>(
    quorum: Quorum,
    voters: impl IntoIterator<Item = (EndpointIndex, &'a EndpointChain)>,
) -> Tally {
    let mut holders: HashMap<BlockRef, EndpointSet> = HashMap::new();
    for (voter, chain) in voters {
        for block in chain.blocks() {
            holders.entry(block).or_default().insert(voter);
        }
    }
    let tip = holders
        .iter()
        .filter(|(_, agreed_by)| quorum.met_by(**agreed_by))
        .max_by_key(|(block, _)| block.height)
        .map(|(block, agreed_by)| QuorumTip { block: *block, agreed_by: *agreed_by });
    let agreeing = match tip {
        Some(tip) => tip.agreed_by,
        None => holders.into_values().max_by_key(EndpointSet::count).unwrap_or_default(),
    };
    Tally { tip, agreeing }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use zaino_primitives::types::{BlockHash, Height};

    use super::*;

    /// Chains as `(floor, [hash byte per height])`, `None` = not voting; expected tip as
    /// `(height, hash byte, agreed_by)` + the agreeing count
    #[test]
    fn the_tip_is_the_highest_block_a_majority_of_chains_hold() {
        type Chain = Option<(u32, &'static [u8])>;
        type Tip = Option<(u32, u8, &'static [usize])>;
        #[rustfmt::skip]
        let cases: [(&str, &[Chain], Tip, usize); 9] = [
            ("N=1 = its tip",                      &[Some((8, &[8, 9, 10]))],                              Some((10, 10, &[0])), 1),
            ("N=2 race: one block ahead",          &[Some((8, &[8, 9, 10])), Some((9, &[9, 10, 11]))],     Some((10, 10, &[0, 1])), 2),
            ("N=3 race: three tips, one chain",    &[Some((8, &[8, 9, 10])), Some((9, &[9, 10, 11])),
                                                     Some((10, &[10, 11, 12]))],                            Some((11, 11, &[1, 2])), 2),
            ("N=3 fork: majority branch wins",     &[Some((8, &[8, 9, 10])), Some((8, &[8, 9, 0xa0])),
                                                     Some((8, &[8, 9, 0xa0]))],                             Some((10, 0xa0, &[1, 2])), 2),
            ("N=3 lag: tip retreats to ancestor",  &[None, Some((7, &[7, 8, 9])), Some((6, &[6, 7, 8]))],   Some((8, 8, &[1, 2])), 2),
            ("N=3 non-voter ignored",              &[Some((8, &[8, 9, 10])), None, None],                    None, 1),
            ("N=3 lone high claim moves nothing",  &[Some((98, &[98, 99, 100])), Some((98, &[98, 99, 100])),
                                                     Some((999, &[0xf0, 0xf1, 0xf2]))],                     Some((100, 100, &[0, 1])), 2),
            ("N=2 split past the window",          &[Some((50, &[50, 51])), Some((10, &[10, 11]))],         None, 1),
            ("N=4 even split: the fork point",      &[Some((8, &[8, 0xa9])), Some((8, &[8, 0xa9])),
                                                     Some((8, &[8, 0xb9])), Some((8, &[8, 0xb9]))],         Some((8, 8, &[0, 1, 2, 3])), 4),
        ];
        for (case, chains, expected, agreeing) in cases {
            let quorum = Quorum::over(NonZeroUsize::new(chains.len()).expect("nz"));
            let chains: Vec<(EndpointIndex, EndpointChain)> = chains
                .iter()
                .enumerate()
                .filter_map(|(index, chain)| {
                    let (floor, bytes) = (*chain)?;
                    let floor = Height::try_from(floor).expect("h");
                    let hashes = bytes.iter().map(|byte| BlockHash::from([*byte; 32]));
                    Some((
                        EndpointIndex::new(index).expect("index"),
                        EndpointChain::of(floor, hashes),
                    ))
                })
                .collect();
            let counted = tally(quorum, chains.iter().map(|(voter, chain)| (*voter, chain)));
            let expected = expected.map(|(height, byte, agreed_by)| QuorumTip {
                block: BlockRef {
                    hash: BlockHash::from([byte; 32]),
                    height: Height::try_from(height).expect("h"),
                },
                agreed_by: EndpointSet::at(agreed_by.iter().copied()),
            });
            assert_eq!((counted.tip, counted.agreeing.count()), (expected, agreeing), "{case}");
        }
    }
}
