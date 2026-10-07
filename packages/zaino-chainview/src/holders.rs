//! Who holds what (`verified-chain.md` §7): holds `(h, hash)` ⇔ its `getblockhash h` = `hash`
//!
//! ```text
//!   poll: claim (its tip) + getblockhash <boundary> + <best> ─┐
//!   header sync: last header of a run off its best chain ─────┼─▶ facts ─┐
//!   VerifiedChain ────────────────────────────────────────────────────────┴─▶ reach, agreement
//! ```
//!
//! - fact = one answer, true when given
//! - fact on the verified chain at `h` ⇒ every verified block ≤ `h` held (hash commits to its
//!   ancestry): one height (`reach`) = all it holds
//! - each poll replaces the last one's facts, a failed poll forgets them (never a stale holder)
//! - pure core: no I/O, no clock
//! - [`Holders::check`]: V1 + V2 (§10)

use std::sync::Arc;

use zaino_header_chain::VerifiedChain;
use zaino_primitives::types::{BlockRef, Height, ReorgDepth};

use crate::endpoints::{Agreement, EndpointIndex, EndpointSet};

#[cfg(test)]
mod fire_drills;
#[cfg(test)]
mod model;

/// `getblockhash` questions per poll (the final boundary, the best)
pub(crate) const ASKED: usize = 2;

/// Every configured validator's standing against one verified chain, configured order
#[derive(Debug, Clone)]
pub(crate) struct Holders {
    depth: ReorgDepth,
    chain: Option<Arc<VerifiedChain>>,
    validators: imbl::Vector<Standing>,
}

/// `reach` = highest verified height its facts hold
#[derive(Debug, Clone, Default)]
struct Standing {
    answers: Option<Answers>,
    reach: Option<Height>,
    agreement: Agreement,
}

/// One poll's facts + header sync's latest since it (`polled`: ascending, at most `ASKED`)
#[derive(Debug, Clone)]
struct Answers {
    claim: BlockRef,
    polled: Vec<BlockRef>,
    served: Option<BlockRef>,
}

impl Answers {
    fn facts(&self) -> impl Iterator<Item = BlockRef> + '_ {
        std::iter::once(self.claim).chain(self.polled.iter().copied()).chain(self.served)
    }
}

impl Holders {
    pub(crate) fn new(configured: usize, depth: ReorgDepth) -> Self {
        let validators = (0..configured).map(|_| Standing::default()).collect();
        Self { depth, chain: None, validators }
    }

    pub(crate) fn best(&self) -> Option<BlockRef> {
        self.chain.as_ref().map(|chain| chain.best())
    }

    /// Next poll's `getblockhash` heights, ascending: the final boundary (`depth` below), the best
    pub(crate) fn asked(&self) -> Vec<Height> {
        let Some(best) = self.best() else { return Vec::new() };
        best.height.checked_sub(self.depth.get()).into_iter().chain([best.height]).collect()
    }

    /// Header chain's word (`None` = nothing verified yet): every standing re-read against it
    pub(crate) fn verified(&mut self, chain: Option<Arc<VerifiedChain>>) {
        self.chain = chain;
        for at in 0..self.validators.len() {
            self.settle(at);
        }
    }

    /// One answered poll: its claim + `polled` (the `getblockhash` answers it gave), replacing
    /// every earlier fact
    pub(crate) fn polled(
        &mut self,
        endpoint: EndpointIndex,
        claim: BlockRef,
        polled: Vec<BlockRef>,
    ) {
        let at = self.configured(endpoint);
        assert!(one_per_asked_height(&polled), "holders: poll answers ascending, at most ASKED");
        self.validators[at].answers = Some(Answers { claim, polled, served: None });
        self.settle(at);
    }

    /// Header sync read `block` off its best chain (lost = ignored: nothing held until a poll)
    pub(crate) fn served(&mut self, endpoint: EndpointIndex, block: BlockRef) {
        let at = self.configured(endpoint);
        let Some(answers) = self.validators[at].answers.as_mut() else { return };
        answers.served = Some(block);
        self.settle(at);
    }

    /// Poll failed (or ejected): holds nothing until its next answer
    pub(crate) fn lost(&mut self, endpoint: EndpointIndex) {
        let at = self.configured(endpoint);
        self.validators[at].answers = None;
        self.settle(at);
    }

    /// Validators holding `block`, a verified best-chain block (V1)
    pub(crate) fn holders(&self, block: BlockRef) -> EndpointSet {
        let chain = self.chain.as_deref().expect("V1: holders asked under a verified chain");
        let verified = chain.hash_at(block.height) == Some(block.hash);
        assert!(verified, "V1: holders asked of a verified block, not {block:?}");
        let holds = |standing: &Standing| standing.reach.is_some_and(|reach| reach >= block.height);
        let held = self.validators.iter().enumerate().filter(|(_, standing)| holds(standing));
        held.filter_map(|(at, _)| EndpointIndex::new(at)).collect()
    }

    pub(crate) fn reach(&self, endpoint: EndpointIndex) -> Option<Height> {
        self.validators.get(endpoint.get())?.reach
    }

    /// Its tip as of its last answered poll (`None` = none since its last failure)
    pub(crate) fn claim(&self, endpoint: EndpointIndex) -> Option<BlockRef> {
        Some(self.validators.get(endpoint.get())?.answers.as_ref()?.claim)
    }

    pub(crate) fn agreement(&self, endpoint: EndpointIndex) -> Agreement {
        self.validators.get(endpoint.get()).map_or(Agreement::Unknown, |s| s.agreement)
    }

    /// V1 + V2 + the facts' shape; panics naming the invariant broken
    pub(crate) fn check(&self) {
        let chain = self.chain.as_deref();
        for (at, standing) in self.validators.iter().enumerate() {
            let answers = standing.answers.as_ref();
            let shaped = answers.is_none_or(|answers| one_per_asked_height(&answers.polled));
            assert!(shaped, "holders: validator {at}'s poll answers ascending, at most ASKED");
            let reach = reach(answers, chain);
            assert_eq!(standing.reach, reach, "V1: validator {at}'s reach = its facts' highest");
            let classified = agreement(answers, reach, chain);
            assert_eq!(standing.agreement, classified, "V2: validator {at}'s agreement");
        }
    }

    fn configured(&self, endpoint: EndpointIndex) -> usize {
        let at = endpoint.get();
        assert!(at < self.validators.len(), "holders: {at} not a configured validator");
        at
    }

    fn settle(&mut self, at: usize) {
        let chain = self.chain.as_deref();
        let standing = &mut self.validators[at];
        standing.reach = reach(standing.answers.as_ref(), chain);
        standing.agreement = agreement(standing.answers.as_ref(), standing.reach, chain);
    }
}

fn one_per_asked_height(polled: &[BlockRef]) -> bool {
    polled.len() <= ASKED && polled.windows(2).all(|pair| pair[0].height < pair[1].height)
}

/// Highest height whose fact = the verified chain's block there
fn reach(answers: Option<&Answers>, chain: Option<&VerifiedChain>) -> Option<Height> {
    let (answers, chain) = (answers?, chain?);
    let on_chain = |fact: &BlockRef| chain.hash_at(fact.height) == Some(fact.hash);
    answers.facts().filter(on_chain).map(|fact| fact.height).max()
}

/// §7, in order: claim = best; holds best, claims higher; claim verified, below best; else diverged
fn agreement(
    answers: Option<&Answers>,
    reach: Option<Height>,
    chain: Option<&VerifiedChain>,
) -> Agreement {
    let (Some(answers), Some(chain)) = (answers, chain) else { return Agreement::Unknown };
    let (best, claim) = (chain.best(), answers.claim);
    let holds_best = reach.is_some_and(|reach| reach >= best.height);
    if claim == best {
        Agreement::Agreed
    } else if holds_best && claim.height > best.height {
        Agreement::Ahead
    } else if claim.height < best.height && chain.hash_at(claim.height) == Some(claim.hash) {
        Agreement::Behind
    } else {
        Agreement::Diverged
    }
}
