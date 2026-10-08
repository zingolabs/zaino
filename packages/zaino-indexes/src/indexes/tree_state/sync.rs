//! Sync-engine wiring for the `tree_state` index: the
//! `(SelfCumulative<OrderedMonoid>, Append)` cell of the sync model.
//!
//! [`TreeStateIndex`] keeps, at every height, one note-commitment frontier per
//! shielded pool. Each height's value is computed from the previous height's
//! (scope is [`SelfCumulative`]), yet every height emits its own disjoint
//! `height → frontiers` entry (composition is [`Append`]). The carry is an
//! **ordered monoid with a measure** ([`OrderedMonoid`]): per-pool leaf counts
//! are the measure (a prefix sum gives each block its absolute start position in
//! each pool's tree), and the [`TreeSegment`] algebra is the monoid, so the
//! append-cumulative bridge builds a whole batch by measure → lift → ordered
//! reduce → projection rather than a serial per-height fold. The hashing is in
//! [`lift`](OrderedMonoidCarry::lift) and parallelises across the batch; each
//! height's frontier is then a pure lookup.
//!
//! The three pools are carried together: the [`Measure`](TreeMeasure),
//! [`Segment`](TreeStateSegment) and value ([`TreeStateValue`]) each bundle a
//! Sapling, an Orchard and an Ironwood component, and every algebra method maps
//! over the three independently. A pool with no leaves yet carries the empty
//! frontier (size 0) — the same state a serve-time read sees as "active but
//! empty"; whether a given height is below a pool's activation is decided at
//! serve time from the network's activation heights (Task 8), not stored here.
//!
//! [`SelfCumulative`]: zaino_sync::descriptor::SelfCumulative
//! [`Append`]: zaino_sync::descriptor::Append
//! [`OrderedMonoid`]: zaino_sync::descriptor::OrderedMonoid

use incrementalmerkletree::frontier::Frontier;
use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node as SaplingNode;
use zaino_primitives::types::NoteCommitment;
use zaino_sync::descriptor::{Append, OrderedMonoid, SelfCumulative};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{
    CumulativeAppend, ExtractCumulative, IndexDef, MergeAppend, OrderedMonoidCarry, Schema,
};

use super::codec::{TreeStateIndex, TreeStateValue, ID};
use super::pools::{ironwood_leaf, orchard_leaf, sapling_leaf};
use super::segment::{TreeSegment, DEPTH};

/// A shielded pool, for error reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display)]
pub enum Pool {
    /// The Sapling note-commitment tree.
    Sapling,
    /// The Orchard note-commitment tree.
    Orchard,
    /// The Ironwood note-commitment tree (shares Orchard's Pallas encoding).
    Ironwood,
}

/// Why building a block's tree-state contribution failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TreeStateError {
    /// A note commitment's 32 bytes were not a canonical field element for its
    /// pool, so no tree leaf exists for it. This is the wire→domain validation
    /// step for a commitment; a canonical source never produces it.
    #[error("non-canonical {pool} note commitment")]
    NonCanonicalCommitment {
        /// The pool whose commitment failed to decode.
        pool: Pool,
    },
    /// Appending this block's leaves would push a pool's tree past
    /// [`DEPTH`] — more than `2^DEPTH` leaves. Only the
    /// sequential ground-truth [`extract`](ExtractCumulative::extract) can raise
    /// it; unreachable on mainnet, where no pool approaches `2^32` notes.
    #[error("the {pool} note-commitment tree exceeded depth {DEPTH}")]
    PoolFull {
        /// The pool whose tree overflowed.
        pool: Pool,
    },
}

/// Per-index block context: the block's height and its note commitments per
/// pool, in chain/transaction order.
#[derive(Debug, Clone)]
pub struct TreeStateCtx {
    /// Block height — the entry key.
    pub height: BlockHeight,
    /// Sapling note commitments (`cmu`) this block adds, in chain order.
    pub sapling_cmus: Vec<NoteCommitment>,
    /// Orchard note commitments (`cmx`) this block adds, in chain order.
    pub orchard_cmxs: Vec<NoteCommitment>,
    /// Ironwood note commitments (`cmx`) this block adds, in chain order.
    pub ironwood_cmxs: Vec<NoteCommitment>,
}

/// One height's entry: the per-pool frontiers after this block.
#[derive(Debug)]
pub struct TreeStateEntry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Per-pool frontiers after this block (value, and the carry to the next).
    pub value: TreeStateValue,
}

/// The ordered-monoid measure: each pool's leaf count. A prefix sum of block
/// measures from the carry gives each block its absolute start position per
/// pool. Additive and commutative (a sum of counts), independent of the
/// non-commutative segment [`combine`](OrderedMonoidCarry::combine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeMeasure {
    /// Sapling leaves.
    pub sapling: u64,
    /// Orchard leaves.
    pub orchard: u64,
    /// Ironwood leaves.
    pub ironwood: u64,
}

/// The ordered-monoid segment: a [`TreeSegment`] per pool, combined
/// independently. Bundling the three keeps one segment per batch so the bridge's
/// reduce and projection run once over all pools.
#[derive(Debug, Clone)]
pub struct TreeStateSegment {
    /// Sapling run summary.
    pub sapling: TreeSegment<SaplingNode>,
    /// Orchard run summary.
    pub orchard: TreeSegment<MerkleHashOrchard>,
    /// Ironwood run summary.
    pub ironwood: TreeSegment<MerkleHashOrchard>,
}

/// `usize` → `u64` for a commitment count. Lossless on every supported platform;
/// a block holds at most `usize` commitments.
fn count(n: usize) -> u64 {
    u64::try_from(n).expect("commitment count fits u64")
}

/// Convert a pool's note commitments to its tree leaves, failing on the first
/// non-canonical encoding. Generic over the pool's leaf type and conversion, so
/// the three pools share one path.
fn leaves_of<H>(
    pool: Pool,
    commitments: &[NoteCommitment],
    convert: impl Fn([u8; 32]) -> Option<H>,
) -> Result<Vec<H>, TreeStateError> {
    commitments
        .iter()
        .map(|commitment| {
            convert((*commitment).into()).ok_or(TreeStateError::NonCanonicalCommitment { pool })
        })
        .collect()
}

/// Append `leaves` to a clone of `prior`, or [`TreeStateError::PoolFull`] if the
/// tree cannot hold them within [`DEPTH`]. The sequential
/// ground truth each pool's frontier must equal.
fn append_pool<H: Hashable + Clone>(
    pool: Pool,
    prior: &Frontier<H, DEPTH>,
    leaves: &[H],
) -> Result<Frontier<H, DEPTH>, TreeStateError> {
    let mut frontier = prior.clone();
    for leaf in leaves {
        if !frontier.append(leaf.clone()) {
            return Err(TreeStateError::PoolFull { pool });
        }
    }
    Ok(frontier)
}

/// The frontier after exactly `size` leaves, read from a combined segment — a
/// pure lookup. `size == 0` is the empty tree. Non-zero sizes rely on the
/// ordered-monoid invariant that every required node was retained during
/// `lift`/`combine`, so the lookup and the depth conversion cannot fail.
fn pool_frontier<H: Hashable + Clone + Send + Sync>(
    segment: &TreeSegment<H>,
    size: u64,
) -> Frontier<H, DEPTH> {
    if size == 0 {
        return Frontier::empty();
    }
    let nonempty = segment
        .frontier_at(size)
        .expect("ordered-monoid lookup: the frontier's nodes were retained by lift/combine");
    Frontier::try_from(nonempty).expect("a frontier read from a depth-bounded segment fits depth")
}

impl IndexDef for TreeStateIndex {
    type Scope = SelfCumulative<OrderedMonoid>;
    type Composition = Append;
    type Delta = TreeStateEntry;
    type BlockContext = TreeStateCtx;

    const NAME: IndexId = ID;
}

impl ExtractCumulative for TreeStateIndex {
    type PriorState = TreeStateValue;
    type Error = TreeStateError;

    // The sequential ground truth: append this block's leaves to the prior
    // frontiers. The ordered-monoid bridge never calls this (it uses the
    // `OrderedMonoidCarry` methods below); it is kept because the trait requires
    // it and because it is the independent per-block fold.
    fn extract(
        ctx: &TreeStateCtx,
        prior: &TreeStateValue,
    ) -> Result<TreeStateEntry, TreeStateError> {
        let sapling = append_pool(
            Pool::Sapling,
            &prior.sapling,
            &leaves_of(Pool::Sapling, &ctx.sapling_cmus, sapling_leaf)?,
        )?;
        let orchard = append_pool(
            Pool::Orchard,
            &prior.orchard,
            &leaves_of(Pool::Orchard, &ctx.orchard_cmxs, orchard_leaf)?,
        )?;
        let ironwood = append_pool(
            Pool::Ironwood,
            &prior.ironwood,
            &leaves_of(Pool::Ironwood, &ctx.ironwood_cmxs, ironwood_leaf)?,
        )?;
        Ok(TreeStateEntry {
            height: ctx.height,
            value: TreeStateValue {
                sapling,
                orchard,
                ironwood,
            },
        })
    }
}

impl MergeAppend for TreeStateIndex {}

impl CumulativeAppend for TreeStateIndex {
    fn initial_carry() -> TreeStateValue {
        TreeStateValue {
            sapling: Frontier::empty(),
            orchard: Frontier::empty(),
            ironwood: Frontier::empty(),
        }
    }

    fn carry(delta: &TreeStateEntry) -> TreeStateValue {
        delta.value.clone()
    }
}

impl OrderedMonoidCarry for TreeStateIndex {
    type Measure = TreeMeasure;

    fn measure_of(ctx: &TreeStateCtx) -> TreeMeasure {
        TreeMeasure {
            sapling: count(ctx.sapling_cmus.len()),
            orchard: count(ctx.orchard_cmxs.len()),
            ironwood: count(ctx.ironwood_cmxs.len()),
        }
    }

    fn measure_add(a: TreeMeasure, b: TreeMeasure) -> TreeMeasure {
        TreeMeasure {
            sapling: a.sapling + b.sapling,
            orchard: a.orchard + b.orchard,
            ironwood: a.ironwood + b.ironwood,
        }
    }

    fn carry_measure(carry: &TreeStateValue) -> TreeMeasure {
        TreeMeasure {
            sapling: carry.sapling.tree_size(),
            orchard: carry.orchard.tree_size(),
            ironwood: carry.ironwood.tree_size(),
        }
    }

    type Segment = TreeStateSegment;

    fn lift(ctx: &TreeStateCtx, start: TreeMeasure) -> Result<TreeStateSegment, TreeStateError> {
        let sapling = leaves_of(Pool::Sapling, &ctx.sapling_cmus, sapling_leaf)?;
        let orchard = leaves_of(Pool::Orchard, &ctx.orchard_cmxs, orchard_leaf)?;
        let ironwood = leaves_of(Pool::Ironwood, &ctx.ironwood_cmxs, ironwood_leaf)?;
        Ok(TreeStateSegment {
            sapling: TreeSegment::lift(&sapling, start.sapling),
            orchard: TreeSegment::lift(&orchard, start.orchard),
            ironwood: TreeSegment::lift(&ironwood, start.ironwood),
        })
    }

    fn carry_segment(carry: &TreeStateValue) -> TreeStateSegment {
        TreeStateSegment {
            sapling: TreeSegment::carry_segment(&carry.sapling),
            orchard: TreeSegment::carry_segment(&carry.orchard),
            ironwood: TreeSegment::carry_segment(&carry.ironwood),
        }
    }

    fn identity() -> TreeStateSegment {
        TreeStateSegment {
            sapling: TreeSegment::empty(),
            orchard: TreeSegment::empty(),
            ironwood: TreeSegment::empty(),
        }
    }

    fn combine(a: TreeStateSegment, b: TreeStateSegment) -> TreeStateSegment {
        TreeStateSegment {
            sapling: TreeSegment::combine(a.sapling, b.sapling),
            orchard: TreeSegment::combine(a.orchard, b.orchard),
            ironwood: TreeSegment::combine(a.ironwood, b.ironwood),
        }
    }

    fn project(full: &TreeStateSegment, at: TreeMeasure) -> TreeStateValue {
        TreeStateValue {
            sapling: pool_frontier(&full.sapling, at.sapling),
            orchard: pool_frontier(&full.orchard, at.orchard),
            ironwood: pool_frontier(&full.ironwood, at.ironwood),
        }
    }

    fn key_of(ctx: &TreeStateCtx) -> BlockHeight {
        ctx.height
    }
}

impl Schema<Vec<TreeStateEntry>> for TreeStateIndex {
    fn into_entries(entries: Vec<TreeStateEntry>) -> Vec<(Self::Key, Self::Value)> {
        entries.into_iter().map(|e| (e.height, e.value)).collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<TreeStateEntry> {
        entries
            .into_iter()
            .map(|(height, value)| TreeStateEntry { height, value })
            .collect()
    }
}
