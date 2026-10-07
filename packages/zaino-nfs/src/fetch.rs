//! Fetching every height from any source (`nfs.md` §12 decision 5; moved from `zaino-sync`'s
//! `ProducerCore`)
//!
//! - Checked = hash + coinbase height + merkle root vs the verified header (`verified-chain.md` §5)
//! - Misanswer → source benched [`BENCH`]
//! - Silence → hedged past [`HEDGE`]
//! - Every source out → retry after [`RETRY`]
//! - Least-loaded source first (ties spread by height)

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zaino_header_chain::{Record, VerifiedChain};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, MerkleRoot, TransactionId};

use crate::core::Output;

pub(crate) const HEDGE: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_secs(1);
const BENCH: Duration = Duration::from_secs(60);

/// Block that passed [`check_block`] (no other constructor)
#[derive(Debug, Clone)]
pub(crate) struct Checked(Arc<Block>);

impl Checked {
    pub(crate) fn block(&self) -> &Arc<Block> {
        &self.0
    }

    pub(crate) fn at(&self) -> BlockRef {
        BlockRef { hash: self.0.header().hash, height: self.0.header().height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Misanswer {
    #[error("served block {got}")]
    WrongBlock { got: BlockHash },
    #[error("served a block whose coinbase says height {got:?}")]
    WrongHeight { got: Height },
    #[error("served a body whose txids do not rebuild the header's merkle root")]
    MerkleRoot,
    #[error("served a body repeating a txid pair (CVE-2012-2459)")]
    Mutated,
}

/// `block` = the one asked for at `height`, its body the one `record` commits to
///
/// - Mismatch = that source misanswered, never an invalid header (ZIP 256: a v5 hash does not
///   commit to authorizing data)
pub(crate) fn check_block(
    block: Block,
    height: Height,
    record: &Record,
) -> Result<Checked, Misanswer> {
    let header = block.header();
    if header.hash != record.hash {
        return Err(Misanswer::WrongBlock { got: header.hash });
    }
    if header.height != height {
        return Err(Misanswer::WrongHeight { got: header.height });
    }
    match merkle_root(&block) {
        None => Err(Misanswer::Mutated),
        Some(root) if root != record.merkle_root => Err(Misanswer::MerkleRoot),
        Some(_) => Ok(Checked(Arc::new(block))),
    }
}

/// `None` = a repeated txid pair (CVE-2012-2459)
pub(crate) fn merkle_root(block: &Block) -> Option<MerkleRoot> {
    let txids: Vec<TransactionId> = block.transactions().iter().map(|tx| tx.txid).collect();
    MerkleRoot::of_txids(&txids)
}

/// Source's answer to an [`Output::Fetch`]
#[derive(Debug, Clone)]
pub(crate) enum Answer {
    Checked(Checked),
    Misanswered(Misanswer),
    Failed,
}

/// Wants (heights the core needs a body for) and who is asked for each
pub(crate) struct Fetcher {
    sources: Vec<Source>,
    wants: BTreeMap<Height, Want>,
}

#[derive(Debug, Clone, Default)]
struct Source {
    load: usize,
    benched_until: Option<Instant>,
}

/// `asked` = asks in flight, `tried` = sources out this round (cleared at `retry_at`)
#[derive(Debug, Clone)]
struct Want {
    hash: BlockHash,
    asked: Vec<(usize, Instant)>,
    tried: Vec<bool>,
    retry_at: Option<Instant>,
}

impl Fetcher {
    pub(crate) fn new(sources: usize) -> Self {
        assert!(sources > 0, "a source to fetch from");
        Self { sources: vec![Source::default(); sources], wants: BTreeMap::new() }
    }

    pub(crate) fn wants(&self, height: Height) -> bool {
        self.wants.contains_key(&height)
    }

    pub(crate) fn want(&mut self, at: BlockRef) {
        let tried = vec![false; self.sources.len()];
        let want = Want { hash: at.hash, asked: Vec::new(), tried, retry_at: None };
        self.wants.entry(at.height).or_insert(want);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(BlockRef) -> bool) {
        self.wants.retain(|height, want| keep(BlockRef { hash: want.hash, height: *height }));
    }

    /// `Some` = the body a live want waited for (stale: the chain moved, or another source won)
    pub(crate) fn answered<F>(
        &mut self,
        from: usize,
        at: BlockRef,
        answer: Answer,
        now: Instant,
        out: &mut Vec<Output<F>>,
    ) -> Option<Checked> {
        let load = &mut self.sources[from].load;
        assert!(*load > 0, "an answer for an ask in flight");
        *load -= 1;
        if let Answer::Checked(block) = &answer {
            assert_eq!(block.at(), at, "N1: a checked block is the one asked for");
        }
        let want = self.wants.get_mut(&at.height).filter(|want| want.hash == at.hash)?;
        want.asked.retain(|(asked, _)| *asked != from);
        match answer {
            Answer::Checked(block) => {
                self.wants.remove(&at.height);
                return Some(block);
            }
            Answer::Misanswered(why) => {
                want.tried[from] = true;
                self.sources[from].benched_until = Some(now + BENCH);
                out.push(Output::Misanswered { from, at, why });
            }
            Answer::Failed => want.tried[from] = true,
        }
        None
    }

    /// Each unasked want asked, silences hedged, a want every source failed retried
    pub(crate) fn ask<F>(&mut self, chain: &VerifiedChain, now: Instant, out: &mut Vec<Output<F>>) {
        for (height, want) in &mut self.wants {
            let ask = match (want.asked.is_empty(), want.retry_at) {
                (true, Some(at)) => at <= now,
                (true, None) => true,
                (false, _) => want.asked.iter().all(|(_, at)| now.duration_since(*at) >= HEDGE),
            };
            if !ask {
                continue;
            }
            if want.retry_at.take().is_some() {
                want.tried.fill(false);
            }
            match pick(&self.sources, want, *height, now) {
                Some(from) => {
                    self.sources[from].load += 1;
                    want.asked.push((from, now));
                    let record = chain.header_at(*height).expect("a wanted height is best");
                    out.push(Output::Fetch { from, height: *height, record });
                }
                None if want.asked.is_empty() => {
                    want.retry_at = Some(now + RETRY);
                    out.push(Output::Unserved { height: *height });
                }
                None => {}
            }
        }
    }

    /// `next` = next height the final stream sends
    pub(crate) fn check(&self, chain: &VerifiedChain, next: Height) {
        for (height, want) in &self.wants {
            assert!(*height >= next, "fetch: wants above the last sent");
            assert_eq!(chain.hash_at(*height), Some(want.hash), "N1: every want is best");
        }
        for (source, state) in self.sources.iter().enumerate() {
            let asks = self.wants.values().flat_map(|want| &want.asked);
            let asked = asks.filter(|(asked, _)| *asked == source).count();
            assert!(state.load >= asked, "fetch: load counts every ask in flight");
        }
    }
}

/// Least-loaded source not asked or out this round and not benched (ties spread by height)
fn pick(sources: &[Source], want: &Want, height: Height, now: Instant) -> Option<usize> {
    let spread = u32::from(height) as usize;
    (0..sources.len())
        .filter(|source| !want.tried[*source])
        .filter(|source| want.asked.iter().all(|(asked, _)| asked != source))
        .filter(|source| sources[*source].benched_until.is_none_or(|until| until <= now))
        .min_by_key(|source| (sources[*source].load, (source + spread) % sources.len()))
}

#[cfg(test)]
mod tests {
    use zaino_header_chain::VerifiedChain;
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::Transaction;

    use super::*;

    /// Chain 0..=3 + a side 2': the asked block passes; a sibling, a block whose coinbase lies
    /// about its height, a body with a txid added (root moves) and a body repeating its last pair
    /// (root kept, CVE-2012-2459) are each refused by name
    #[test]
    fn only_the_asked_block_with_the_body_its_header_commits_to_passes() {
        let mut chain = Chain::new();
        let one = chain.mine(chain.genesis().hash);
        let paid: Vec<Transaction> = (0..3u8)
            .map(|n| {
                let mut tx = chain.block(chain.genesis().hash).transactions()[0].clone();
                tx.txid = [n + 1; 32].into();
                tx
            })
            .collect();
        let two = chain.mine_with(one.hash, paid);
        let tip = chain.mine(two.hash);
        let side = chain.mine(one.hash);
        let verified = VerifiedChain::regtest(&chain.path(tip.hash));
        let h2 = Height::try_from(2u32).expect("h");
        let record = verified.header_at(h2).expect("on the best chain");
        let block = |hash: BlockHash| chain.block(hash).clone();
        let with_txs = |txs: Vec<Transaction>| Block::new(block(two.hash).header().clone(), txs);
        let mut header = block(two.hash).header().clone();
        header.height = Height::try_from(9u32).expect("h");
        let lying = Block::new(header, block(two.hash).transactions().to_vec());
        let txs = block(two.hash).transactions().to_vec();
        let added = [txs.clone(), vec![txs[0].clone()]].concat();
        let repeated = [txs.clone(), vec![txs[2].clone()]].concat();

        let passed = check_block(block(two.hash), h2, &record).expect("the asked block");
        assert_eq!(passed.at(), BlockRef { hash: two.hash, height: h2 });
        let refused = [
            (block(side.hash), Misanswer::WrongBlock { got: side.hash }),
            (lying, Misanswer::WrongHeight { got: Height::try_from(9u32).expect("h") }),
            (with_txs(added), Misanswer::MerkleRoot),
            (with_txs(repeated), Misanswer::Mutated),
        ];
        for (served, why) in refused {
            assert_eq!(check_block(served, h2, &record).map(|c| c.at()), Err(why));
        }
    }

    /// Fire drills (`verified-chain.md` §10 layer 4): wants 2, 3 on chain 0..=3, each asked of
    /// one of two sources
    #[test]
    fn every_fetch_check_fires_on_its_planted_bug() {
        let mut chain = Chain::new();
        let tip = chain.extend(chain.genesis().hash, 3);
        let path = chain.path(tip.hash);
        let verified = VerifiedChain::regtest(&path);
        let at =
            |h: usize| BlockRef { hash: path[h].header().hash, height: path[h].header().height };
        let now = Instant::now();
        let valid = || {
            let mut fetcher = Fetcher::new(2);
            fetcher.want(at(2));
            fetcher.want(at(3));
            fetcher.ask::<()>(&verified, now, &mut Vec::new());
            fetcher
        };
        let next = at(2).height;
        valid().check(&verified, next);
        let loads: Vec<usize> = valid().sources.iter().map(|source| source.load).collect();
        assert_eq!(loads, [1, 1], "the planted state");

        type Plant<'a> = Box<dyn Fn(&mut Fetcher) + 'a>;
        let drills: Vec<(&str, Plant)> = vec![
            ("fetch: wants above the last sent", Box::new(|f| f.want(at(1)))),
            (
                "N1: every want is best",
                Box::new(|f| f.wants.values_mut().for_each(|want| want.hash = BlockHash::ZERO)),
            ),
            (
                "fetch: load counts every ask in flight",
                Box::new(|f| f.sources.iter_mut().for_each(|source| source.load = 0)),
            ),
        ];
        for (expected, plant) in drills {
            let mut fetcher = valid();
            plant(&mut fetcher);
            let message = crate::fired(|| fetcher.check(&verified, next)).unwrap_or_default();
            assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
        }

        let checked = |h: usize| {
            let record = verified.header_at(at(h).height).expect("best");
            check_block(path[h].clone(), at(h).height, &record).expect("honest")
        };
        let answer = |load: usize, at: BlockRef, answer: Answer| {
            let mut fetcher = valid();
            fetcher.sources.iter_mut().for_each(|source| source.load = load);
            crate::fired(|| drop(fetcher.answered::<()>(0, at, answer, now, &mut Vec::new())))
        };
        let preconditions = [
            ("a source to fetch from", crate::fired(|| drop(Fetcher::new(0)))),
            ("an answer for an ask in flight", answer(0, at(2), Answer::Failed)),
            (
                "N1: a checked block is the one asked for",
                answer(1, at(3), Answer::Checked(checked(2))),
            ),
        ];
        for (expected, message) in preconditions {
            let message = message.unwrap_or_default();
            assert!(message.contains(expected), "expected {expected:?}, fired {message:?}");
        }
    }
}
