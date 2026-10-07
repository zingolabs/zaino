//! [`ProducerCore`]: the [`VerifiedChain`] + what the sink holds → the steps sent next and the
//! fetches feeding them (`verified-chain.md` §9, §10)
//!
//! - Pure: no I/O, no clock (`now` = an input); fetch, check, send = the driver's
//! - One finality = the chain's final tip
//! - Fork = first delivered non-final height whose hash != `hash_at` (a comparison, no walk-back)
//! - Any source → any block ([`check_block`](super::checked::check_block) decides)
//! - Misanswer → source benched [`BENCH`]; silence → hedged past [`HEDGE`]; every source out →
//!   retry after [`RETRY`]

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zaino_header_chain::{Record, VerifiedChain};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};

use super::checked::{Checked, Misanswer};
use super::ProduceError;

/// Ask another source once every ask for a block is this old
pub(crate) const HEDGE: Duration = Duration::from_secs(15);
/// Pause before a block every source failed is asked again
pub(crate) const RETRY: Duration = Duration::from_secs(1);
/// Source skipped this long after a misanswer
pub(crate) const BENCH: Duration = Duration::from_secs(60);

/// - `announced` = last height every subscriber holds final
/// - `window` = delivered non-final, contiguous from `announced + 1`
/// - `unchecked` = durable tips no final tip covers yet (P6: nothing delivered meanwhile)
pub(crate) struct ProducerCore {
    lookahead: usize,
    chain: Option<Arc<VerifiedChain>>,
    final_seen: Option<BlockRef>,
    announced: Option<Height>,
    window: VecDeque<Checked>,
    unchecked: Vec<BlockRef>,
    ready: BTreeMap<Height, Checked>,
    wants: BTreeMap<Height, Want>,
    sources: Vec<Source>,
}

#[derive(Debug, Clone, Default)]
struct Source {
    load: usize,
    benched_until: Option<Instant>,
}

/// - `asked` = asks in flight; `tried` = sources out this round (cleared at `retry_at`)
#[derive(Debug, Clone)]
struct Want {
    hash: BlockHash,
    asked: Vec<(usize, Instant)>,
    tried: Vec<bool>,
    retry_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(crate) enum Input {
    Chain(Arc<VerifiedChain>),
    Answer { source: usize, height: Height, hash: BlockHash, answer: Answer },
    Tick,
}

#[derive(Debug, Clone)]
pub(crate) enum Answer {
    Checked(Checked),
    Misanswered(Misanswer),
    Failed,
}

/// - Sink steps (`Apply`, `Finalized`, `Reorg`) sent exactly in list order, fetches in any
/// - `Reorg.dropped` = delivered non-final blocks off the new best, from `fork` up
#[derive(Debug, Clone)]
pub(crate) enum Output {
    Apply { block: Arc<Block>, finalized: bool },
    Finalized(Height),
    Reorg { fork: Height, dropped: usize },
    Fetch { source: usize, height: Height, record: Record },
    Misanswered { source: usize, height: Height, why: Misanswer },
    Unserved { height: Height },
}

impl ProducerCore {
    /// - `durable` = every subscriber's durable tip (delivery starts after the rearmost)
    /// - `lookahead` = blocks fetched ahead of the next one delivered
    pub(crate) fn new(
        sources: usize,
        lookahead: usize,
        durable: impl IntoIterator<Item = Option<BlockRef>>,
    ) -> Self {
        assert!(sources > 0, "a source to fetch from");
        assert!(lookahead > 0, "at least one block in flight");
        let durable: Vec<Option<BlockRef>> = durable.into_iter().collect();
        let rearmost = durable.iter().map(|tip| tip.map(|tip| tip.height)).min();
        let rearmost = rearmost.expect("a sink with a subscriber");
        Self {
            lookahead,
            chain: None,
            final_seen: None,
            announced: rearmost,
            window: VecDeque::new(),
            unchecked: durable.into_iter().flatten().collect(),
            ready: BTreeMap::new(),
            wants: BTreeMap::new(),
            sources: vec![Source::default(); sources],
        }
    }

    /// `Err` = an index's durable block is not the final chain's (resync required)
    pub(crate) fn step(&mut self, input: Input, now: Instant) -> Result<Vec<Output>, ProduceError> {
        let mut out = Vec::new();
        match input {
            Input::Chain(chain) => self.follow(chain, &mut out)?,
            Input::Answer { source, height, hash, answer } => {
                self.answered(source, height, hash, answer, now, &mut out)
            }
            Input::Tick => {}
        }
        self.deliver(&mut out);
        self.fetch(now, &mut out);
        Ok(out)
    }

    /// Last block delivered, final or not (`None` = none)
    pub(crate) fn delivered(&self) -> Option<Height> {
        self.window.back().map(Checked::height).or(self.announced)
    }

    fn next(&self) -> Height {
        self.delivered().map_or(Height::GENESIS, Height::next)
    }

    fn is_final(&self, height: Height) -> bool {
        self.final_seen.is_some_and(|tip| height <= tip.height)
    }

    fn follow(
        &mut self,
        chain: Arc<VerifiedChain>,
        out: &mut Vec<Output>,
    ) -> Result<(), ProduceError> {
        let final_tip = chain.final_tip();
        let height = |tip: Option<BlockRef>| tip.map(|tip| tip.height);
        assert!(height(final_tip) >= height(self.final_seen), "P3: the final tip never moves back");
        if let Some(seen) = self.final_seen {
            assert_eq!(
                chain.hash_at(seen.height),
                Some(seen.hash),
                "P3: a final block never changes"
            );
        }
        self.final_seen = final_tip;
        self.check_durable(&chain)?;

        // final + still best: announced before any reorg (a reorg replays from above them)
        while let Some(front) = self.window.front() {
            if !self.is_final(front.height()) || chain.hash_at(front.height()) != Some(front.hash())
            {
                break;
            }
            out.push(Output::Finalized(front.height()));
            self.announced = Some(front.height());
            self.window.pop_front();
        }
        let off_best = |block: &Checked| chain.hash_at(block.height()) != Some(block.hash());
        if let Some(fork) = self.window.iter().position(off_best) {
            let dropped = self.window.len() - fork;
            out.push(Output::Reorg { fork: self.window[fork].height(), dropped });
            // subscribers replay from `announced + 1`: the still-best prefix comes from here
            for block in self.window.drain(..).take(fork) {
                self.ready.insert(block.height(), block);
            }
        }
        self.ready.retain(|height, block| chain.hash_at(*height) == Some(block.hash()));
        self.wants.retain(|height, want| chain.hash_at(*height) == Some(want.hash));
        self.chain = Some(chain);
        Ok(())
    }

    /// P6: every durable tip a final tip covers = the chain's block there; the rest wait
    fn check_durable(&mut self, chain: &VerifiedChain) -> Result<(), ProduceError> {
        let final_height = chain.final_tip().map(|tip| tip.height);
        for tip in &self.unchecked {
            if Some(tip.height) > final_height {
                continue;
            }
            match chain.hash_at(tip.height) {
                Some(got) if got == tip.hash => {}
                got => {
                    let got = got.expect("at or below the final tip");
                    let (height, expected) = (tip.height, tip.hash);
                    return Err(ProduceError::Diverged { height, expected, got });
                }
            }
        }
        self.unchecked.retain(|tip| Some(tip.height) > final_height);
        Ok(())
    }

    fn answered(
        &mut self,
        source: usize,
        height: Height,
        hash: BlockHash,
        answer: Answer,
        now: Instant,
        out: &mut Vec<Output>,
    ) {
        let load = &mut self.sources[source].load;
        assert!(*load > 0, "an answer for an ask in flight");
        *load -= 1;
        if let Answer::Checked(block) = &answer {
            let asked = (block.height(), block.hash()) == (height, hash);
            assert!(asked, "P1: a checked block is the one asked for");
        }
        // stale = the chain moved off it, or another source answered first
        let Some(want) = self.wants.get_mut(&height).filter(|want| want.hash == hash) else {
            return;
        };
        want.asked.retain(|(asked, _)| *asked != source);
        match answer {
            Answer::Checked(block) => {
                self.wants.remove(&height);
                self.ready.insert(height, block);
            }
            Answer::Misanswered(why) => {
                want.tried[source] = true;
                self.sources[source].benched_until = Some(now + BENCH);
                out.push(Output::Misanswered { source, height, why });
            }
            Answer::Failed => want.tried[source] = true,
        }
    }

    /// Contiguous checked blocks from `next()`, each still `hash_at` its height
    fn deliver(&mut self, out: &mut Vec<Output>) {
        let Some(chain) = self.chain.clone() else { return };
        if !self.unchecked.is_empty() {
            return;
        }
        loop {
            let next = self.next();
            let Some(hash) = chain.hash_at(next) else { return };
            let Some(block) = self.ready.remove(&next) else { return };
            assert_eq!(block.hash(), hash, "P1: ready blocks follow the chain");
            let finalized = self.is_final(next);
            out.push(Output::Apply { block: Arc::clone(block.block()), finalized });
            if finalized {
                assert!(self.window.is_empty(), "P2: a final Apply only with nothing pending");
                self.announced = Some(next);
            } else {
                self.window.push_back(block);
            }
        }
    }

    /// `lookahead` heights from `next()` wanted, each unasked want asked, silences hedged
    fn fetch(&mut self, now: Instant, out: &mut Vec<Output>) {
        let Some(chain) = self.chain.clone() else { return };
        let next = self.next();
        let best = chain.best().height;
        for height in next.up_to(best).take(self.lookahead) {
            if self.ready.contains_key(&height) || self.wants.contains_key(&height) {
                continue;
            }
            let hash = chain.hash_at(height).expect("at or below the best tip");
            let tried = vec![false; self.sources.len()];
            self.wants.insert(height, Want { hash, asked: Vec::new(), tried, retry_at: None });
        }
        let heights: Vec<Height> = self.wants.keys().copied().collect();
        for height in heights {
            let want = &self.wants[&height];
            let ask = match (want.asked.is_empty(), want.retry_at) {
                (true, Some(at)) if at > now => false,
                (true, _) => true,
                (false, _) => want.asked.iter().all(|(_, at)| now.duration_since(*at) >= HEDGE),
            };
            if !ask {
                continue;
            }
            let want = self.wants.get_mut(&height).expect("listed above");
            if want.retry_at.take().is_some() {
                want.tried.fill(false);
            }
            match pick(&self.sources, want, height, now) {
                Some(source) => {
                    self.sources[source].load += 1;
                    want.asked.push((source, now));
                    let record = chain.header_at(height).expect("a wanted height is best");
                    out.push(Output::Fetch { source, height, record });
                }
                None if want.asked.is_empty() => {
                    want.retry_at = Some(now + RETRY);
                    out.push(Output::Unserved { height });
                }
                None => {}
            }
        }
    }

    /// P1–P3, P6 and the fetch bookkeeping; panics naming the invariant broken
    pub(crate) fn check(&self) {
        let Some(chain) = &self.chain else {
            assert!(self.window.is_empty() && self.ready.is_empty(), "P1: nothing before a chain");
            return;
        };
        assert_eq!(chain.final_tip(), self.final_seen, "P3: one finality, the chain's");
        let on_chain = |block: &Checked| chain.hash_at(block.height()) == Some(block.hash());
        assert!(self.window.iter().all(on_chain), "P1: every delivered non-final block is best");
        assert!(self.ready.values().all(on_chain), "P1: every ready block is best");
        let from = self.announced.map_or(Height::GENESIS, Height::next);
        let contiguous =
            (0..).zip(&self.window).all(|(i, b)| from.checked_add(i) == Some(b.height()));
        assert!(contiguous, "P2: window contiguous from the last final height");
        let non_final = self.window.iter().all(|block| !self.is_final(block.height()));
        assert!(non_final, "P3: no final block left pending");
        if self.unchecked.is_empty() {
            let final_height = self.final_seen.map(|tip| tip.height);
            assert!(self.announced <= final_height, "P3: only final heights announced final");
        } else {
            assert!(
                self.window.is_empty(),
                "P6: nothing delivered before every durable tip checks"
            );
        }
        let next = self.next();
        let past = |height: &Height| *height >= next;
        assert!(self.ready.keys().all(past), "P2: ready blocks above the delivered tip");
        assert!(self.wants.keys().all(past), "P2: wants above the delivered tip");
        for (height, want) in &self.wants {
            assert_eq!(chain.hash_at(*height), Some(want.hash), "P1: every want is best");
            assert!(!self.ready.contains_key(height), "fetch: a ready block is not wanted");
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
mod fire_drills;
#[cfg(test)]
mod model;
