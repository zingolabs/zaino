//! Header trees at random against a naive model (usage.md's rules, recomputed from scratch)
//!
//! - anchored at genesis, any nBits (most work != highest), trusted + untrusted runs
//! - `HeaderSync`'s discipline: nothing offered above `ceiling(RUN)`, a trusted run's last vouched
//! - untrusted: model predicts the rules a `MockChain` header breaks (short context, edited time)

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroU32;

use proptest::prelude::*;
use zaino_primitives::protocol::median_time_past;
use zaino_primitives::types::{BlockRef, FinalityDepth, Netgroup, ECLIPSE_NETGROUPS};
use zaino_source::testing::{all_at, MockChain};
use zcash_protocol::consensus::{BlockHeight, H0};
use zcash_protocol::BlockHash;

use crate::rules::{header_work, needed_context, Rejected, CONTEXT};
use crate::{
    decode_header, BestTip, Header, HeaderChain, HeaderChainSnapshot, Inserted, Params, Record,
    Witness,
};

const DEPTH: u32 = 3;
/// Headers a fetch may reach past `depth` above the finalized tip (`HeaderSync`'s batch)
const RUN: u32 = 6;
const SIDE_NODES: usize = 4 * DEPTH as usize;
const SIDE_TIPS: usize = 32;
/// Finals the chain keeps by height: `2 · DEPTH`, at least the rules' `CONTEXT`
const KEPT: usize = if 2 * (DEPTH as usize) > CONTEXT { 2 * DEPTH as usize } else { CONTEXT };
/// Work ≈ 16 · 512 · 4096 a header
const BITS: [u32; 3] = [0x200f_0f0f, 0x1f7f_ffff, 0x1f0f_ffff];
/// The local clock: past every mined header (the 2 h future bound never fires)
const NOW: i64 = i64::MAX / 2;

/// - `Extend`: `bits.len()` headers on mined `parent`, `gap` s apart, `trusted` = a validator's
///   run (its last accepted header vouched, then finality)
/// - `Orphan`: two headers on `parent`, only the second offered (parent unknown)
/// - `Spray`: `branches` single-header side branches off live nodes (prune pressure, H4)
/// - `Witness`: a poll's claim (`peers: None`), or `inv`s from each netgroup set in `peers` (any
///   mined header, held or not)
#[derive(Debug, Clone)]
enum Move {
    Extend { parent: usize, gap: u32, bits: Vec<usize>, trusted: bool },
    Witness { mined: usize, peers: Option<u8> },
    Orphan { parent: usize },
    Spray { from: usize, branches: usize },
    Reinsert { mined: usize, trusted: bool },
    Finalize,
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let any = || 0usize..100_000;
    let bits = prop::collection::vec(
        prop_oneof![6 => Just(0usize), 2 => Just(1usize), 1 => Just(2usize)],
        1..=4,
    );
    prop::collection::vec(
        prop_oneof![
            10 => (any(), 1u32..=300, bits, prop::bool::ANY).prop_map(
                |(parent, gap, bits, trusted)| Move::Extend { parent, gap, bits, trusted }
            ),
            2 => any().prop_map(|mined| Move::Witness { mined, peers: None }),
            4 => (any(), prop::num::u8::ANY)
                .prop_map(|(mined, peers)| Move::Witness { mined, peers: Some(peers) }),
            1 => any().prop_map(|parent| Move::Orphan { parent }),
            1 => (any(), 1usize..=40).prop_map(|(from, branches)| Move::Spray { from, branches }),
            1 => (any(), prop::bool::ANY)
                .prop_map(|(mined, trusted)| Move::Reinsert { mined, trusted }),
            5 => Just(Move::Finalize),
        ],
        1..100,
    )
}

#[derive(Debug, Clone, Copy)]
struct Alive {
    record: Record,
    height: BlockHeight,
    parent: BlockHash,
    received: u64,
}

/// - `finalized_headers[h]` = the finalized record at height `h`
/// - `vouched` / `carried` = live headers vouched / announced per netgroup (their live ancestors
///   too, derived)
/// - `last_vouched` = (height, work) of the highest header ever vouched, else the anchor
struct Model {
    mined: Vec<Header>,
    by_hash: HashMap<BlockHash, Header>,
    alive: HashMap<BlockHash, Alive>,
    vouched: HashSet<BlockHash>,
    carried: HashMap<BlockHash, BTreeSet<u8>>,
    finalized_headers: Vec<Record>,
    received: u64,
    last_vouched: (BlockHeight, u128),
}

impl Model {
    fn finalized_tip(&self) -> Option<BlockRef> {
        let record = self.finalized_headers.last()?;
        Some(BlockRef { hash: record.hash, height: height(self.finalized_headers.len() - 1) })
    }

    /// Lowest finalized height the chain still answers by height
    fn kept_from(&self) -> usize {
        self.finalized_headers.len().saturating_sub(KEPT)
    }

    fn leaves(&self) -> Vec<BlockHash> {
        let parents: Vec<BlockHash> = self.alive.values().map(|alive| alive.parent).collect();
        self.alive.keys().copied().filter(|hash| !parents.contains(hash)).collect()
    }

    fn best(&self) -> Option<BestTip> {
        let leaf = self.leaves().into_iter().max_by_key(|leaf| {
            let alive = &self.alive[leaf];
            (alive.record.cumulative_work, Reverse(alive.received))
        });
        match leaf {
            Some(leaf) => {
                let alive = &self.alive[&leaf];
                let block = BlockRef { hash: leaf, height: alive.height };
                Some(BestTip { block, cumulative_work: alive.record.cumulative_work })
            }
            None => self.finalized_tip().map(|block| BestTip {
                block,
                cumulative_work: self.finalized_headers.last().expect("tip").cumulative_work,
            }),
        }
    }

    /// Finals, then the live best branch: index = height
    fn best_path(&self) -> Vec<Record> {
        let mut above = Vec::new();
        let mut at = self.best().map(|best| best.block.hash);
        while let Some(alive) = at.and_then(|hash| self.alive.get(&hash)) {
            above.push(alive.record);
            at = Some(alive.parent);
        }
        above.reverse();
        let mut path = self.finalized_headers.clone();
        path.extend(above);
        path
    }

    fn tail(&self) -> &[Record] {
        &self.finalized_headers[self.kept_from()..]
    }

    /// Height + work of `prev`'s child, or the linkage rejection
    fn parent(&self, prev: BlockHash) -> Result<(BlockHeight, u128), Rejected> {
        if let Some(alive) = self.alive.get(&prev) {
            return Ok((alive.height + 1, alive.record.cumulative_work));
        }
        if let Some(tip) = self.finalized_tip().filter(|tip| tip.hash == prev) {
            return Ok((
                tip.height + 1,
                self.finalized_headers.last().expect("tip").cumulative_work,
            ));
        }
        match self.tail().iter().any(|record| record.hash == prev) {
            true => Err(Rejected::BelowFinalized),
            false => Err(Rejected::Orphan),
        }
    }

    /// Median time past of a child of `prev` (the builder's: a valid MockChain time)
    fn median_time_past(&self, prev: BlockHash) -> u32 {
        let ancestors = std::iter::successors(self.by_hash.get(&prev), |header| {
            self.by_hash.get(&header.prev_hash())
        });
        median_time_past(ancestors.map(Header::time)).expect("`prev` mined")
    }

    /// Ancestors of a child of `prev` the chain holds: live ones down to the finalized tip, then
    /// the kept finalized headers, up to `CONTEXT`
    fn held_ancestors(&self, prev: BlockHash) -> usize {
        let mut live = 0;
        let mut at = prev;
        while let Some(alive) = self.alive.get(&at) {
            live += 1;
            at = alive.parent;
        }
        (live + self.tail().len()).min(CONTEXT)
    }

    /// What the chain owes an untrusted `header`: the rules a valid `MockChain` header can break
    /// here (too few ancestors held, a time not past the median: an edited one), else `offer`
    fn offer_untrusted(&mut self, header: &Header, params: &Params) -> Result<Inserted, Rejected> {
        let prev = header.prev_hash();
        let known = self.alive.contains_key(&header.hash())
            || self.tail().iter().any(|record| record.hash == header.hash());
        let parent = self.parent(prev);
        if let (false, Ok((at, _))) = (known, parent) {
            let (held, needed) = (self.held_ancestors(prev), needed_context(params, at));
            if held < needed {
                return Err(Rejected::ShortContext { held, needed });
            }
            let median_time_past = self.median_time_past(prev);
            if header.time() <= median_time_past {
                return Err(Rejected::TimeTooEarly { time: header.time(), median_time_past });
            }
        }
        self.offer(header)
    }

    /// What the chain owes `header`, applied to the model
    fn offer(&mut self, header: &Header) -> Result<Inserted, Rejected> {
        let hash = header.hash();
        self.by_hash.entry(hash).or_insert_with(|| header.clone());
        if self.alive.contains_key(&hash) || self.tail().iter().any(|r| r.hash == hash) {
            return Ok(Inserted::Known);
        }
        let (at, parent_work) = self.parent(header.prev_hash())?;
        let own = header_work(header.bits()).expect("palette bits");
        let record = Record {
            hash,
            time: header.time(),
            bits: header.bits(),
            cumulative_work: parent_work + own,
        };
        self.received += 1;
        let alive =
            Alive { record, height: at, parent: header.prev_hash(), received: self.received };
        self.alive.insert(hash, alive);
        let inserted = match self.best().map(|best| best.block.hash) == Some(hash) {
            true => Inserted::Best,
            false => Inserted::Side,
        };
        self.evict();
        Ok(inserted)
    }

    fn evict(&mut self) {
        loop {
            let best = self.best().map(|best| best.block.hash);
            let on_best = self.best_path().len() - self.finalized_headers.len();
            let side_nodes = self.alive.len() - on_best;
            let side_tips = self.leaves().len() - usize::from(best.is_some());
            if side_nodes <= SIDE_NODES && side_tips <= SIDE_TIPS {
                return;
            }
            let victim = self
                .leaves()
                .into_iter()
                .filter(|leaf| Some(*leaf) != best)
                .min_by_key(|leaf| {
                    let alive = &self.alive[leaf];
                    (alive.record.cumulative_work, Reverse(alive.received))
                })
                .expect("a side leaf");
            let gone = self.alive.remove(&victim).expect("a live leaf");
            let (vouched, carried) = (self.vouched.remove(&victim), self.carried.remove(&victim));
            if !self.alive.contains_key(&gone.parent) {
                continue;
            }
            if vouched {
                self.vouched.insert(gone.parent);
            }
            if let Some(netgroups) = carried {
                self.carried.entry(gone.parent).or_default().extend(netgroups);
            }
        }
    }

    /// `peer: None` = a trusted validator
    fn witness(&mut self, hash: BlockHash, peer: Option<u8>) {
        let Some(alive) = self.alive.get(&hash) else { return };
        match peer {
            Some(netgroup) => drop(self.carried.entry(hash).or_default().insert(netgroup)),
            None => {
                if alive.height > self.last_vouched.0 {
                    let work = header_work(alive.record.bits).expect("palette bits");
                    self.last_vouched = (alive.height, work);
                }
                self.vouched.insert(hash);
            }
        }
    }

    /// Live headers some vouched header descends from, and each one's netgroups (its and every
    /// live descendant's)
    fn witnessed(&self) -> (HashSet<BlockHash>, HashMap<BlockHash, BTreeSet<u8>>) {
        let ancestors = |from: BlockHash| {
            std::iter::successors(Some(from), |at| self.alive.get(at).map(|alive| alive.parent))
                .take_while(|at| self.alive.contains_key(at))
        };
        let vouched = self.vouched.iter().flat_map(|hash| ancestors(*hash)).collect();
        let mut carried: HashMap<BlockHash, BTreeSet<u8>> = HashMap::new();
        for (hash, netgroups) in &self.carried {
            for at in ancestors(*hash) {
                carried.entry(at).or_default().extend(netgroups);
            }
        }
        (vouched, carried)
    }

    /// Highest best-path height settled, else the finalized tip's
    fn settled_height(&self) -> Option<BlockHeight> {
        let (vouched, carried) = self.witnessed();
        let path = self.best_path();
        let own = |record: &Record| header_work(record.bits).expect("palette bits");
        let unvouched =
            path[self.finalized_headers.len()..].iter().filter(|r| !vouched.contains(&r.hash));
        let collapsed = unvouched.into_iter().any(|record| own(record) * 2 < self.last_vouched.1);
        let quorate = |hash| carried.get(hash).is_some_and(|n| n.len() > ECLIPSE_NETGROUPS);
        let top = (self.finalized_headers.len()..path.len()).rev().find(|h| {
            let hash = &path[*h].hash;
            vouched.contains(hash) || (quorate(hash) && !collapsed)
        });
        top.map(height).or(self.finalized_tip().map(|tip| tip.height))
    }

    /// Highest height `HeaderSync` fetches
    fn ceiling(&self) -> BlockHeight {
        height(self.finalized_headers.len()) + DEPTH + RUN - 1
    }

    fn finalizable(&self) -> Option<BlockRef> {
        let best = self.best()?;
        let below = BlockHeight::from(u32::from(best.block.height).checked_sub(DEPTH)?);
        let boundary = below.min(self.settled_height()?);
        let index = usize::try_from(u32::from(boundary)).expect("u32 fits usize");
        (index >= self.finalized_headers.len())
            .then(|| BlockRef { hash: self.best_path()[index].hash, height: boundary })
    }

    fn finalize(&mut self, through: BlockRef) {
        let path = self.best_path();
        let through_index = usize::try_from(u32::from(through.height)).expect("u32 fits usize");
        self.finalized_headers = path[..=through_index].to_vec();
        let descends = |model: &Model, mut at: BlockHash| loop {
            match model.alive.get(&at) {
                _ if at == through.hash => return true,
                Some(alive) if alive.height > through.height => at = alive.parent,
                _ => return false,
            }
        };
        let keep: Vec<BlockHash> = self
            .alive
            .iter()
            .filter(|(hash, alive)| alive.height > through.height && descends(self, **hash))
            .map(|(hash, _)| *hash)
            .collect();
        self.alive.retain(|hash, _| keep.contains(hash));
        self.vouched.retain(|hash| keep.contains(hash));
        self.carried.retain(|hash, _| keep.contains(hash));
    }

    /// `holds`: on the best path (older finalized headers than the chain keeps = on it by
    /// definition), or a live side node
    fn holds(&self, at: BlockRef, path: &[Record]) -> bool {
        let index = usize::try_from(u32::from(at.height)).expect("u32 fits usize");
        let finalized_unkept = index < self.kept_from();
        let on_best = path.get(index).is_some_and(|record| record.hash == at.hash);
        let side = self.alive.get(&at.hash).is_some_and(|alive| alive.height == at.height);
        finalized_unkept || on_best || side
    }

    /// zcashd's locator in closed form: 12 consecutive heights from the best, then best − 9 − 2^k
    /// (k ≥ 2), each clamped to the floor, ending at the floor
    fn locator(&self) -> Vec<BlockHash> {
        let Some(best) = self.best() else { return Vec::new() };
        let (best, path) = (i64::from(u32::from(best.block.height)), self.best_path());
        let floor = self.finalized_tip().map_or(0, |tip| i64::from(u32::from(tip.height)));
        let consecutive = (0..12).map(|back| best - back);
        let doubling = (2..40).map(|k| best - 9 - (1i64 << k));
        let mut locator = Vec::new();
        for at in consecutive.chain(doubling).map(|at| at.max(floor)) {
            locator.push(path[usize::try_from(at).expect("clamped to floor >= 0")].hash);
            if at == floor {
                break;
            }
        }
        locator
    }
}

/// Best-path index → height
fn height(h: usize) -> BlockHeight {
    BlockHeight::from(u32::try_from(h).expect("model chains stay far below u32::MAX"))
}

/// Headers as the chain receives them: `MockChain` bytes, decoded and hashed here
///
/// - `templates[hash]` = block whose bytes `hash`'s header was cut from (itself unless edited)
/// - edited = `prev_hash` + `time` rewritten (a parent `MockChain` never held)
struct Builder {
    chain: MockChain,
    templates: HashMap<BlockHash, BlockRef>,
}

impl Builder {
    fn genesis(&mut self) -> Header {
        let genesis = self.chain.genesis();
        self.received(genesis, |_| {})
    }

    fn mine(&mut self, model: &Model, prev: BlockHash, time: u32, bits: u32) -> Header {
        let template = self.templates[&prev];
        let exact = template.hash == prev && time > model.median_time_past(prev);
        let mined = self.chain.branch(template).mine(|b| match exact {
            true => b.time(time).bits(bits),
            false => b.bits(bits),
        });
        let mined = mined.tip();
        let header = self.received(mined, |raw| {
            if !exact {
                raw[4..36].copy_from_slice(&prev.0);
                raw[100..104].copy_from_slice(&time.to_le_bytes());
            }
        });
        assert_eq!((header.prev_hash(), header.time(), header.bits()), (prev, time, bits));
        header
    }

    fn received(&mut self, block: BlockRef, edit: impl FnOnce(&mut Vec<u8>)) -> Header {
        let mut raw = self.chain.header_bytes(block.hash);
        let held = raw.clone();
        edit(&mut raw);
        let header = decode_header(&raw).expect("well-formed regtest header");
        let unedited = raw == held;
        assert_eq!(header.hash() == block.hash, unedited, "hash = SHA-256d of the bytes");
        self.templates.insert(header.hash(), block);
        header
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// After every move: chain answers = model (every insert's outcome, best tip and work,
    /// finalized tip, boundary, ceiling, live tree size, every height and header of the published
    /// chain, its locator, vouched per height and `holds`)
    ///
    /// - `check()` holds
    /// - old published chains never change
    #[test]
    fn random_header_trees_answer_like_the_naive_model(moves in moves()) {
        run(moves)?;
    }
}

fn run(moves: Vec<Move>) -> Result<(), TestCaseError> {
    let chain = MockChain::regtest().varied_work();
    let mut builder = Builder { chain, templates: HashMap::new() };
    let genesis = builder.genesis();
    let depth = FinalityDepth::new(NonZeroU32::new(DEPTH).expect("nz"));
    let mut chain = HeaderChain::new(depth);
    chain.anchor(&genesis, H0).expect("MockChain nBits valid");
    let anchor = Record {
        hash: genesis.hash(),
        time: genesis.time(),
        bits: genesis.bits(),
        cumulative_work: 0,
    };
    let mut model = Model {
        mined: vec![genesis.clone()],
        by_hash: HashMap::from([(genesis.hash(), genesis.clone())]),
        alive: HashMap::new(),
        vouched: HashSet::new(),
        carried: HashMap::new(),
        finalized_headers: vec![anchor],
        received: 0,
        last_vouched: (H0, header_work(genesis.bits()).expect("regtest limit")),
    };
    /// Chain published one move earlier + what it answered then (`kept_from` = lowest height by
    /// height, `holds` = each probe's answer)
    struct Published {
        snapshot: HeaderChainSnapshot,
        path: Vec<Record>,
        kept_from: usize,
        holds: Vec<(BlockRef, bool)>,
    }
    let mut published: Option<Published> = None;

    // - driver's: nothing above the ceiling offered (H9)
    // - untrusted = every rule run first
    let params = Params::regtest(all_at(height(1)));
    let params = params.with_genesis(genesis.hash()).any_bits();
    let offer = |chain: &mut HeaderChain,
                 model: &mut Model,
                 header: &Header,
                 trusted: bool,
                 context: &str|
     -> Result<bool, TestCaseError> {
        let at = model.parent(header.prev_hash());
        if at.is_ok_and(|(at, _)| at > model.ceiling()) {
            return Ok(false);
        }
        let (expected, got) = match trusted {
            true => (model.offer(header), chain.insert(header)),
            false => (
                model.offer_untrusted(header, &params),
                chain.insert_untrusted(header, &params, NOW),
            ),
        };
        prop_assert_eq!(got, expected, "{}: insert {:?} (trusted {})", context, header, trusted);
        chain.check();
        Ok(expected.is_ok())
    };
    let mine = |builder: &mut Builder, model: &mut Model, prev: BlockHash, time: u32, bits: u32| {
        let header = builder.mine(model, prev, time, bits);
        model.mined.push(header.clone());
        model.by_hash.insert(header.hash(), header.clone());
        header
    };

    for (step, next) in moves.into_iter().enumerate() {
        let context = format!("step {step} {next:?}");
        match next {
            Move::Extend { parent, gap, bits, trusted } => {
                let parent = model.mined[parent % model.mined.len()].clone();
                let (mut prev, mut time) = (parent.hash(), parent.time());
                let mut last = None;
                for palette in bits {
                    time += gap;
                    let header = mine(&mut builder, &mut model, prev, time, BITS[palette]);
                    if !offer(&mut chain, &mut model, &header, trusted, &context)? {
                        break;
                    }
                    prev = header.hash();
                    last = Some(BlockRef { hash: prev, height: builder.templates[&prev].height });
                }
                if let (true, Some(last)) = (trusted, last) {
                    chain.witness(last.hash, Witness::Trusted);
                    model.witness(last.hash, None);
                    let boundary = model.finalizable();
                    prop_assert_eq!(chain.finalizable(), boundary, "{}: run's boundary", context);
                    if let Some(boundary) = boundary {
                        chain.finalize(boundary);
                        model.finalize(boundary);
                    }
                    // liveness: finalized = min(vouched, best − depth), no further input
                    prop_assert_eq!(chain.finalizable(), None, "{}: finalized caught up", context);
                    let best = chain.best().expect("anchored").block;
                    let deep = u32::from(best.height).checked_sub(DEPTH).map(BlockHeight::from);
                    let finalized_height = chain.finalized_tip().map(|tip| tip.height);
                    let caught = best != last || finalized_height >= deep;
                    prop_assert!(
                        caught,
                        "{}: a run ending at the best {:?} = finalized at best − depth, got {:?}",
                        context,
                        best,
                        finalized_height
                    );
                }
            }
            Move::Witness { mined, peers } => {
                let hash = model.mined[mined % model.mined.len()].hash();
                let netgroups = peers.map(|mask| (0..8u8).filter(move |n| mask & (1 << n) != 0));
                if netgroups.is_none() {
                    chain.witness(hash, Witness::Trusted);
                    model.witness(hash, None);
                }
                for n in netgroups.into_iter().flatten() {
                    chain.witness(hash, Witness::Peer(Netgroup::V4([n, 0])));
                    model.witness(hash, Some(n));
                }
            }
            Move::Orphan { parent } => {
                let parent = model.mined[parent % model.mined.len()].clone();
                let skipped =
                    mine(&mut builder, &mut model, parent.hash(), parent.time() + 1, BITS[0]);
                let child =
                    mine(&mut builder, &mut model, skipped.hash(), skipped.time() + 1, BITS[0]);
                let offered = offer(&mut chain, &mut model, &child, false, &context)?;
                prop_assert!(!offered, "{}: orphan {:?} accepted", context, child);
            }
            Move::Spray { from, branches } => {
                let mut live: Vec<(u64, BlockHash)> =
                    model.alive.iter().map(|(hash, alive)| (alive.received, *hash)).collect();
                live.sort_unstable_by_key(|(received, _)| *received);
                for branch in 0..branches.min(live.len() * 4) {
                    let parent = live[(from + branch) % live.len()].1;
                    let branch = u32::try_from(branch).expect("branches < 4 · live tree");
                    let time = model.by_hash[&parent].time() + 1 + branch;
                    let header = mine(&mut builder, &mut model, parent, time, BITS[0]);
                    offer(&mut chain, &mut model, &header, true, &context)?;
                }
            }
            Move::Reinsert { mined, trusted } => {
                let again = model.mined[mined % model.mined.len()].clone();
                offer(&mut chain, &mut model, &again, trusted, &context)?;
            }
            Move::Finalize => {
                let boundary = model.finalizable();
                prop_assert_eq!(chain.finalizable(), boundary, "{}: boundary", context);
                if let Some(boundary) = boundary {
                    chain.finalize(boundary);
                    model.finalize(boundary);
                }
                prop_assert_eq!(
                    chain.finalizable(),
                    None,
                    "{}: one call reaches the target",
                    context
                );
            }
        }

        chain.check();
        prop_assert_eq!(chain.best(), model.best(), "{}: best, chain vs model", context);
        let finalized_tip = model.finalized_tip();
        prop_assert_eq!(chain.finalized_tip(), finalized_tip, "{}: finalized tip", context);
        prop_assert_eq!(chain.finalizable(), model.finalizable(), "{}: boundary", context);
        prop_assert_eq!(chain.ceiling(RUN), model.ceiling(), "{}: ceiling", context);
        prop_assert_eq!(chain.tree_len(), model.alive.len(), "{}: live tree size", context);
        let non_finalized = model.best_path().len() - model.finalized_headers.len();
        let bound = usize::try_from(DEPTH + RUN).expect("u32 fits usize");
        prop_assert!(
            non_finalized <= bound,
            "{}: non-finalized on best within depth + run (H9): {} > {}",
            context,
            non_finalized,
            bound
        );
        let path = model.best_path();
        let kept_from = model.kept_from();
        let verified = chain.snapshot().expect("anchored");
        let answers = |chain: &HeaderChainSnapshot, path: &[Record]| -> Vec<Option<Record>> {
            (0..path.len()).map(|at| chain.header_at(height(at))).collect()
        };
        let expected = |path: &[Record], kept_from: usize| -> Vec<Option<Record>> {
            path.iter()
                .enumerate()
                .map(|(at, record)| (at >= kept_from).then_some(*record))
                .collect()
        };
        let answered = answers(&verified, &path);
        prop_assert_eq!(answered, expected(&path, kept_from), "{}: header by height", context);
        let above = verified.hash_at(height(path.len()));
        prop_assert_eq!(above, None, "{}: above best", context);
        prop_assert_eq!(verified.locator(), model.locator(), "{}: locator", context);
        let (vouched, _) = model.witnessed();
        let finalized = model.finalized_headers.len();
        let model_vouched: Vec<bool> =
            (0..path.len()).map(|at| at < finalized || vouched.contains(&path[at].hash)).collect();
        let chain_vouched: Vec<bool> =
            (0..path.len()).map(|at| verified.vouched(height(at))).collect();
        prop_assert_eq!(chain_vouched, model_vouched, "{}: vouched per best-path height", context);

        let mut holds = Vec::new();
        for header in &model.mined {
            let hash = header.hash();
            let at = BlockRef { hash, height: builder.templates[&hash].height };
            let below = u32::from(at.height).checked_sub(1).map(BlockHeight::from);
            for at in below.into_iter().chain([at.height, at.height + 1]) {
                let at = BlockRef { hash, height: at };
                let held = verified.holds(at);
                prop_assert_eq!(held, model.holds(at, &path), "{}: holds {:?}", context, at);
                holds.push((at, held));
            }
        }

        if let Some(Published {
            snapshot: old,
            path: old_path,
            kept_from: old_kept_from,
            holds: old_holds,
        }) = &published
        {
            let answered = answers(old, old_path);
            prop_assert_eq!(
                answered,
                expected(old_path, *old_kept_from),
                "{}: a published chain never changes (H5)",
                context
            );
            let still: Vec<(BlockRef, bool)> =
                old_holds.iter().map(|(at, _)| (*at, old.holds(*at))).collect();
            prop_assert_eq!(&still, old_holds, "{}: nor its side blocks (H5)", context);
            let (now, then) = (verified.finalized_tip(), old.finalized_tip());
            prop_assert!(
                now.height >= then.height,
                "{}: finalized moves up (H2): {:?} after {:?}",
                context,
                now,
                then
            );
        }
        published = Some(Published { snapshot: verified, path, kept_from, holds });
    }
    Ok(())
}
