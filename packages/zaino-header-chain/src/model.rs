//! Header trees at random against a naive model
//!
//! - trusted headers (no rule run) with any nBits, so work varies per branch and the most work is
//!   not the highest; anchored at genesis (work counted from it)
//! - model: every header ever mined (moves draw parents from it), the live tree as a plain map,
//!   the final chain as a list (the chain keeps only its newest `2 · DEPTH`); best = the max-work
//!   leaf (first received on a tie), eviction = the lowest-work side leaf (last received on a
//!   tie), both recomputed from scratch
//! - driver discipline (`HeaderSync`'s): nothing offered above `ceiling(RUN)`; a trusted run =
//!   its last header vouched, then finality
//! - vouched oracle: every live ancestor of a vouched header (an evicted one passes it to its
//!   parent); final target = min(highest vouched on best, best − depth)
//! - checked after every move: `check()`, best, final tip, boundary, tree size + its bound (H9),
//!   every height of the published chain (kept finals and above; older finals = on best by
//!   definition), its locator, its forks and their branches, `holds` for every header ever mined,
//!   and the chain published one move earlier still answering as it did (H5)
//! - forks oracle: each side leaf's mined ancestry against the best path (common prefix = `from`)

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;

use proptest::prelude::*;
use zaino_primitives::testing::MockChain;
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth};

use crate::rules::{median_time, Rejected, MEDIAN_SPAN};
use crate::target::{expand, work};
use crate::{decode_header, BestTip, Fork, Header, HeaderChain, Inserted, Record, VerifiedChain};

const DEPTH: u32 = 3;
/// Headers a fetch may reach past `depth` above the final tip (`HeaderSync`'s batch)
const RUN: u32 = 6;
const SIDE_NODES: usize = 4 * DEPTH as usize;
const SIDE_TIPS: usize = 32;
/// Finals the chain keeps by height
const KEPT: usize = 2 * DEPTH as usize;
/// Work ≈ 16 · 512 · 4096 a header
const BITS: [u32; 3] = [0x200f_0f0f, 0x1f7f_ffff, 0x1f0f_ffff];

/// - `Extend`: `bits.len()` headers on mined `parent`, `gap` s apart; `trusted` = a validator's
///   run (its last accepted header vouched, then finality)
/// - `Orphan`: two headers on `parent`, only the second offered (parent unknown)
/// - `Spray`: `branches` single-header side branches off live nodes (prune pressure, H4)
/// - `Vouch`: a poll's claim (any mined header, held or not)
#[derive(Debug, Clone)]
enum Move {
    Extend { parent: usize, gap: u32, bits: Vec<usize>, trusted: bool },
    Vouch { mined: usize },
    Orphan { parent: usize },
    Spray { from: usize, branches: usize },
    Reinsert { mined: usize },
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
            2 => any().prop_map(|mined| Move::Vouch { mined }),
            1 => any().prop_map(|parent| Move::Orphan { parent }),
            1 => (any(), 1usize..=40).prop_map(|(from, branches)| Move::Spray { from, branches }),
            1 => any().prop_map(|mined| Move::Reinsert { mined }),
            3 => Just(Move::Finalize),
        ],
        1..100,
    )
}

#[derive(Debug, Clone, Copy)]
struct Alive {
    record: Record,
    height: Height,
    parent: BlockHash,
    received: u64,
}

/// - `finals[h]` = the final record at height `h`
/// - `vouched` = live headers vouched (their live ancestors too, derived)
struct Model {
    mined: Vec<Header>,
    by_hash: HashMap<BlockHash, Header>,
    alive: HashMap<BlockHash, Alive>,
    vouched: HashSet<BlockHash>,
    finals: Vec<Record>,
    received: u64,
}

impl Model {
    fn final_tip(&self) -> Option<BlockRef> {
        let record = self.finals.last()?;
        Some(BlockRef { hash: record.hash, height: height(self.finals.len() as u32 - 1) })
    }

    /// Lowest final height the chain still answers by height
    fn kept_from(&self) -> usize {
        self.finals.len().saturating_sub(KEPT)
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
            None => self.final_tip().map(|block| BestTip {
                block,
                cumulative_work: self.finals.last().expect("tip").cumulative_work,
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
        let mut path = self.finals.clone();
        path.extend(above);
        path
    }

    fn tail(&self) -> &[Record] {
        &self.finals[self.kept_from()..]
    }

    /// Height + work of `prev`'s child, or the linkage rejection
    fn parent(&self, prev: BlockHash) -> Result<(Height, u128), Rejected> {
        if let Some(alive) = self.alive.get(&prev) {
            return Ok((alive.height.next(), alive.record.cumulative_work));
        }
        if let Some(tip) = self.final_tip().filter(|tip| tip.hash == prev) {
            return Ok((tip.height.next(), self.finals.last().expect("tip").cumulative_work));
        }
        match self.tail().iter().any(|record| record.hash == prev) {
            true => Err(Rejected::BelowFinal),
            false => Err(Rejected::Orphan),
        }
    }

    /// Median time past of a child of `prev` (the builder's: a valid MockChain time)
    fn median_time_past(&self, prev: BlockHash) -> u32 {
        let mut times = Vec::new();
        let mut at = prev;
        while times.len() < MEDIAN_SPAN && at != BlockHash::ZERO {
            let header = &self.by_hash[&at];
            times.push(header.time());
            at = header.prev_hash();
        }
        median_time(times.into_iter())
    }

    /// What the chain owes `header`, applied to the model
    fn offer(&mut self, header: &Header) -> Result<Inserted, Rejected> {
        let hash = header.hash();
        self.by_hash.entry(hash).or_insert_with(|| header.clone());
        if self.alive.contains_key(&hash) || self.tail().iter().any(|r| r.hash == hash) {
            return Ok(Inserted::Known);
        }
        let (at, parent_work) = self.parent(header.prev_hash())?;
        let own = work(expand(header.bits()).expect("palette bits")).expect("fits");
        let record = Record {
            hash,
            merkle_root: header.merkle_root(),
            time: header.time(),
            bits: header.bits(),
            cumulative_work: parent_work + own,
        };
        let before = self.best();
        self.received += 1;
        let alive =
            Alive { record, height: at, parent: header.prev_hash(), received: self.received };
        self.alive.insert(hash, alive);
        let inserted = match self.best().map(|best| best.block.hash) == Some(hash) {
            true => Inserted::Best {
                reorg: before.is_some_and(|best| best.block.hash != header.prev_hash()),
            },
            false => Inserted::Side,
        };
        self.evict();
        Ok(inserted)
    }

    fn evict(&mut self) {
        loop {
            let best = self.best().map(|best| best.block.hash);
            let on_best = self.best_path().len() - self.finals.len();
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
            if self.vouched.remove(&victim) && self.alive.contains_key(&gone.parent) {
                self.vouched.insert(gone.parent);
            }
        }
    }

    fn vouch(&mut self, block: BlockRef) {
        if self.alive.get(&block.hash).is_some_and(|alive| alive.height == block.height) {
            self.vouched.insert(block.hash);
        }
    }

    /// Highest best-path height some vouched header descends from, else the final tip's
    fn vouched_height(&self) -> Option<Height> {
        let mut covered = HashSet::new();
        for vouched in &self.vouched {
            let mut at = *vouched;
            while let Some(alive) = self.alive.get(&at) {
                covered.insert(at);
                at = alive.parent;
            }
        }
        let path = self.best_path();
        let top = (self.finals.len()..path.len()).rev().find(|h| covered.contains(&path[*h].hash));
        top.map(|h| height(h as u32)).or(self.final_tip().map(|tip| tip.height))
    }

    /// Highest height `HeaderSync` fetches
    fn ceiling(&self) -> Height {
        height(self.finals.len() as u32 + DEPTH + RUN - 1)
    }

    fn finalizable(&self) -> Option<BlockRef> {
        let best = self.best()?;
        let boundary = best.block.height.checked_sub(DEPTH)?.min(self.vouched_height()?);
        (u32::from(boundary) >= self.finals.len() as u32).then(|| BlockRef {
            hash: self.best_path()[u32::from(boundary) as usize].hash,
            height: boundary,
        })
    }

    fn finalize(&mut self, through: BlockRef) {
        let path = self.best_path();
        self.finals = path[..=u32::from(through.height) as usize].to_vec();
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
    }

    /// `holds`: on the best path (older finals than the chain keeps = on it by definition), or a
    /// live side node
    fn holds(&self, at: BlockRef, path: &[Record]) -> bool {
        let index = u32::from(at.height) as usize;
        let final_unkept = index < self.kept_from();
        let on_best = path.get(index).is_some_and(|record| record.hash == at.hash);
        let side = self.alive.get(&at.hash).is_some_and(|alive| alive.height == at.height);
        final_unkept || on_best || side
    }

    /// zcashd's locator in closed form: 12 consecutive heights from the best, then best − 9 − 2^k
    /// (k ≥ 2), each clamped to the floor, ending at the floor
    fn locator(&self) -> Vec<BlockHash> {
        let Some(best) = self.best() else { return Vec::new() };
        let (best, path) = (i64::from(u32::from(best.block.height)), self.best_path());
        let floor = self.final_tip().map_or(0, |tip| i64::from(u32::from(tip.height)));
        let consecutive = (0..12).map(|back| best - back);
        let doubling = (2..40).map(|k| best - 9 - (1i64 << k));
        let mut locator = Vec::new();
        for at in consecutive.chain(doubling).map(|at| at.max(floor)) {
            locator.push(path[at as usize].hash);
            if at == floor {
                break;
            }
        }
        locator
    }

    /// Per side leaf: its fork (`from` = end of the common prefix of its mined ancestry and the
    /// best path) + its branch above `from`; most work first, first received on a tie
    fn forks(&self) -> Vec<(Fork, Vec<BlockRef>)> {
        let path = self.best_path();
        let best = self.best().map(|best| best.block.hash);
        let mut forks: Vec<(u64, Fork, Vec<BlockRef>)> = Vec::new();
        for leaf in self.leaves().into_iter().filter(|leaf| Some(*leaf) != best) {
            let mut ascending = vec![leaf];
            while let Some(header) = self.by_hash.get(ascending.last().expect("leaf")) {
                if header.prev_hash() == BlockHash::ZERO {
                    break;
                }
                ascending.push(header.prev_hash());
            }
            ascending.reverse();
            let shared = ascending.iter().zip(&path).take_while(|(at, on)| **at == on.hash).count();
            let at = |h: usize| BlockRef { hash: ascending[h], height: height(h as u32) };
            let alive = self.alive[&leaf];
            let work = alive.record.cumulative_work;
            let tip = at(ascending.len() - 1);
            let fork = Fork { from: at(shared - 1), tip, cumulative_work: work };
            forks.push((alive.received, fork, (shared..ascending.len()).map(at).collect()));
        }
        forks.sort_by_key(|(received, fork, _)| (Reverse(fork.cumulative_work), *received));
        forks.into_iter().map(|(_, fork, branch)| (fork, branch)).collect()
    }
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("in range")
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
                raw[4..36].copy_from_slice(&<[u8; 32]>::from(prev));
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

    /// After every move the chain answers exactly like the model: every insert's outcome, best
    /// tip and work, final tip, boundary, live tree, every height and header of the published
    /// chain, its locator, forks, branches and `holds`; `check()` holds; old published chains
    /// never change
    #[test]
    fn random_header_trees_answer_like_the_naive_model(moves in moves()) {
        run(moves);
    }
}

fn run(moves: Vec<Move>) {
    let chain = MockChain::regtest().varied_work();
    let mut builder = Builder { chain, templates: HashMap::new() };
    let genesis = builder.genesis();
    let depth = ReorgDepth::new(NonZeroU32::new(DEPTH).expect("nz"));
    let mut chain = HeaderChain::new(depth);
    chain.anchor(&genesis, Height::GENESIS);
    let anchor = Record {
        hash: genesis.hash(),
        merkle_root: genesis.merkle_root(),
        time: genesis.time(),
        bits: genesis.bits(),
        cumulative_work: 0,
    };
    let mut model = Model {
        mined: vec![genesis.clone()],
        by_hash: HashMap::from([(genesis.hash(), genesis.clone())]),
        alive: HashMap::new(),
        vouched: HashSet::new(),
        finals: vec![anchor],
        received: 0,
    };
    // (published chain, the model's path then, the lowest height it answered by height, forks)
    let mut published: Option<(VerifiedChain, Vec<Record>, usize, Vec<Fork>)> = None;

    // the driver's: nothing above the ceiling offered (H9)
    let offer = |chain: &mut HeaderChain, model: &mut Model, header: &Header, context: &str| {
        let at = model.parent(header.prev_hash());
        if at.is_ok_and(|(at, _)| at > model.ceiling()) {
            return false;
        }
        let expected = model.offer(header);
        assert_eq!(chain.insert(header), expected, "{context}: insert {header:?}");
        chain.check();
        expected.is_ok()
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
                    if !offer(&mut chain, &mut model, &header, &context) {
                        break;
                    }
                    prev = header.hash();
                    last = Some(BlockRef { hash: prev, height: builder.templates[&prev].height });
                }
                if let (true, Some(last)) = (trusted, last) {
                    chain.vouch(last);
                    model.vouch(last);
                    let boundary = model.finalizable();
                    assert_eq!(chain.finalizable(), boundary, "{context}: run's boundary");
                    if let Some(boundary) = boundary {
                        chain.finalize(boundary);
                        model.finalize(boundary);
                    }
                    // liveness: final = min(vouched, best − depth), no further input
                    assert_eq!(chain.finalizable(), None, "{context}: final caught up");
                    let best = chain.best().expect("anchored").block;
                    let deep = best.height.checked_sub(DEPTH);
                    let final_height = chain.final_tip().map(|tip| tip.height);
                    let caught = best != last || final_height >= deep;
                    assert!(caught, "{context}: a run ending at the best = final at best − depth");
                }
            }
            Move::Vouch { mined } => {
                let header = &model.mined[mined % model.mined.len()];
                let at = BlockRef {
                    hash: header.hash(),
                    height: builder.templates[&header.hash()].height,
                };
                chain.vouch(at);
                model.vouch(at);
            }
            Move::Orphan { parent } => {
                let parent = model.mined[parent % model.mined.len()].clone();
                let skipped =
                    mine(&mut builder, &mut model, parent.hash(), parent.time() + 1, BITS[0]);
                let child =
                    mine(&mut builder, &mut model, skipped.hash(), skipped.time() + 1, BITS[0]);
                assert!(!offer(&mut chain, &mut model, &child, &context), "{context}: orphan");
            }
            Move::Spray { from, branches } => {
                let mut live: Vec<(u64, BlockHash)> =
                    model.alive.iter().map(|(hash, alive)| (alive.received, *hash)).collect();
                live.sort_unstable();
                for branch in 0..branches.min(live.len() * 4) {
                    let parent = live[(from + branch) % live.len()].1;
                    let time = model.by_hash[&parent].time() + 1 + branch as u32;
                    let header = mine(&mut builder, &mut model, parent, time, BITS[0]);
                    offer(&mut chain, &mut model, &header, &context);
                }
            }
            Move::Reinsert { mined } => {
                let again = model.mined[mined % model.mined.len()].clone();
                offer(&mut chain, &mut model, &again, &context);
            }
            Move::Finalize => {
                let boundary = model.finalizable();
                assert_eq!(chain.finalizable(), boundary, "{context}");
                if let Some(boundary) = boundary {
                    chain.finalize(boundary);
                    model.finalize(boundary);
                }
                assert_eq!(chain.finalizable(), None, "{context}: one call reaches the target");
            }
        }

        chain.check();
        assert_eq!(chain.best(), model.best(), "{context}: best");
        assert_eq!(chain.final_tip(), model.final_tip(), "{context}: final tip");
        assert_eq!(chain.finalizable(), model.finalizable(), "{context}: boundary");
        assert_eq!(chain.ceiling(RUN), model.ceiling(), "{context}: ceiling");
        assert_eq!(chain.tree_len(), model.alive.len(), "{context}: live tree");
        let unfinal = model.best_path().len() - model.finals.len();
        let bound = (DEPTH + RUN) as usize;
        assert!(unfinal <= bound, "{context}: H9, {unfinal} unfinal on best > {bound}");
        let path = model.best_path();
        let kept_from = model.kept_from();
        let verified = chain.verified().expect("anchored");
        let answers = |chain: &VerifiedChain, path: &[Record]| -> Vec<Option<Record>> {
            (0..path.len() as u32).map(|at| chain.header_at(height(at))).collect()
        };
        let expected = |path: &[Record], kept_from: usize| -> Vec<Option<Record>> {
            path.iter()
                .enumerate()
                .map(|(at, record)| (at >= kept_from).then_some(*record))
                .collect()
        };
        assert_eq!(answers(&verified, &path), expected(&path, kept_from), "{context}: path");
        assert_eq!(verified.hash_at(height(path.len() as u32)), None, "{context}: above best");
        assert_eq!(verified.locator(), model.locator(), "{context}: locator");

        let (forks, branches): (Vec<Fork>, Vec<Vec<BlockRef>>) = model.forks().into_iter().unzip();
        assert_eq!(verified.forks(), forks, "{context}: forks");
        let held: Vec<Vec<BlockRef>> = forks.iter().map(|f| verified.branch(&f.tip.hash)).collect();
        assert_eq!(held, branches, "{context}: branch of each fork");
        let best = verified.best();
        assert_eq!(verified.branch(&best.hash), [], "{context}: the best tip = no side branch");
        for header in &model.mined {
            let hash = header.hash();
            let at = BlockRef { hash, height: builder.templates[&hash].height };
            for at in at.height.checked_sub(1).into_iter().chain([at.height, at.height.next()]) {
                let at = BlockRef { hash, height: at };
                assert_eq!(verified.holds(at), model.holds(at, &path), "{context}: holds {at:?}");
            }
        }

        if let Some((old, old_path, old_kept_from, old_forks)) = &published {
            let answered = answers(old, old_path);
            assert_eq!(
                answered,
                expected(old_path, *old_kept_from),
                "{context}: H5, a published chain never changes"
            );
            assert_eq!(old.forks(), *old_forks, "{context}: H5, nor its forks");
            let final_height = |chain: &VerifiedChain| chain.final_tip().height;
            assert!(final_height(&verified) >= final_height(old), "{context}: H2, final moves up");
        }
        published = Some((verified, path, kept_from, forks));
    }
}
