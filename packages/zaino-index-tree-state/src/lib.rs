//! Height → commitment-tree state (Sapling, Orchard, Ironwood); subtree index → subtree root
//!
//! # Data structure: incremental Merkle tree, retained nodes in per-level flat arrays (`nodes.rs`)
//!
//! ```text
//! <dir>/
//!   MANIFEST                  committed tip, every table's seal = the commit point
//!   heights.dat               one record per height (slot = height)
//!   sapling/ orchard/ ironwood/
//!     l00.dat … l31.dat       one sequence per tree level, 32 B per node
//!     subtrees.dat            one entry per completed 2^16-leaf subtree (slot = subtree index)
//!
//! heights.dat record, 48 B:   hash 32 ‖ time u32 ‖ sapling u32 ‖ orchard u32 ‖ ironwood u32 (LE)
//! subtrees.dat entry, 36 B:   root 32 ‖ completing height u32 LE
//! level sequences:            l00 = every leaf (slot = index); l01..l31 = even index only
//!                             (slot = index / 2)
//! ```
//!
//! - textbook incremental tree = frontier only (latest state); full tree = 2N nodes
//! - kept here: every leaf + every even-index internal node = exactly where any past frontier's
//!   ommers come from (ommer = left sibling = even) → ≈48 B per commitment (32 leaf + ≈16 internal)
//! - positional: no keys stored, one record read per node
//! - read-heavy (librustzcash asks 1:1 with `GetBlockRange`) → no replay per read, no hashing
//! - one block = its height record + the nodes and subtree roots it completes; blocks above the
//!   durable tip = `zaino_persistence::Tiered` (same positions, RAM only)
//!
//! # Lookup (`ReadView::treestate`)
//!
//! ```text
//! height h ──▶ heights[h] ──▶ hash, time, size s per pool        (held records first)
//!                                 │ per pool, position p = s - 1
//!                                 ▼
//!              leaf     l00[p]
//!              ommers   each set bit ℓ of p: node (ℓ, (p >> ℓ) - 1)  (≤ 32 reads, no hashing,
//!                         ℓ = 0 → l00[p - 1]                          held nodes first)
//!                         ℓ ≥ 1 → lℓ[((p >> ℓ) - 1) / 2]
//!                                 │
//!                                 ▼
//!              frontier → `write_commitment_tree` → `Treestate`
//!
//! subtree i ──▶ subtrees[i] ──▶ root, completing height ──▶ heights ──▶ completing hash
//! ```
//!
//! Storage: `zaino_persistence` port (`docs/design/persistence-engine.md`),
//! `docs/design/index-data-structures.md` §3

use zaino_persistence::{Changes, IndexKind, Schema, SequenceId, SequenceRead, Width};
use zaino_primitives::types::{Height, ShieldedPool};
use zcash_protocol::consensus::NetworkType;

mod fold;
mod heights;
mod index_writer;
mod nodes;
mod serve;
mod subtrees;
mod view;

pub use index_writer::TreeStateIndexWriter;
pub use serve::{PoolActivations, ServeError, TreeStateService};
pub use view::ReadView;

use heights::TreeStateHeight;
use nodes::{MERKLE_DEPTH, NODE};

/// On-disk layout version
const FORMAT: u16 = 1;

const HEIGHTS: SequenceId = SequenceId(0);
/// Level sequences + subtrees, per pool
const POOL_TABLES: u16 = MERKLE_DEPTH as u16 + 1;

/// `pool`'s first table (pools in `ShieldedPool::ALL` order, after `HEIGHTS`)
fn pool_base(pool: ShieldedPool) -> u16 {
    let (at, _) = (0u16..).zip(ShieldedPool::ALL).find(|&(_, each)| each == pool).expect("in ALL");
    1 + at * POOL_TABLES
}

/// `<pool>/l{level:02}`: retained nodes of `level`, slot = [`nodes::slot`]
pub(crate) fn level_table(pool: ShieldedPool, level: u8) -> SequenceId {
    SequenceId(pool_base(pool) + u16::from(level))
}

/// `<pool>/subtrees`: slot = subtree index
pub(crate) fn subtree_table(pool: ShieldedPool) -> SequenceId {
    SequenceId(pool_base(pool) + u16::from(MERKLE_DEPTH))
}

/// What a tree-state index directory holds (also what `zainod verify` checks it against)
pub fn schema(network: NetworkType) -> Schema {
    let schema = Schema::new(IndexKind::TreeState, FORMAT, network).with_sequence(
        HEIGHTS,
        "heights",
        Width::fixed(heights::RECORD as u32),
    );
    ShieldedPool::ALL.into_iter().fold(schema, |schema, pool| {
        let levels = (0..MERKLE_DEPTH).fold(schema, |schema, level| {
            let name = format!("{pool}/l{level:02}");
            schema.with_sequence(level_table(pool, level), &name, Width::fixed(NODE as u32))
        });
        let entry = Width::fixed(subtrees::ENTRY as u32);
        levels.with_sequence(subtree_table(pool), &format!("{pool}/subtrees"), entry)
    })
}

/// `at`'s height record (`None` above `view`'s tip)
pub(crate) fn height_record(view: &impl SequenceRead, at: Height) -> Option<TreeStateHeight> {
    let bytes = view.record(HEIGHTS, u64::from(at))?;
    Some(heights::decode(bytes[..].try_into().expect("RECORD bytes")))
}

#[derive(Debug, thiserror::Error)]
pub enum IndexWriterError {
    /// Stored nodes will not rebuild a frontier of the recorded size (a fold bug)
    #[error("tree-state store is inconsistent at size {size}")]
    Inconsistent { size: u64 },

    /// Non-canonical field element off the wire
    #[error("block {height} carries an uncommittable note commitment")]
    Commitment { height: Height },
}

/// Appends `records` to `table` at its entry in `ends` (per sequence, moved on), each at its own
/// slot (slot != end = a fold bug)
fn append_in_order<R: AsRef<[u8]>>(
    changes: &mut Changes,
    ends: &mut [u64],
    table: SequenceId,
    records: impl Iterator<Item = (u64, R)>,
) {
    let end = &mut ends[usize::from(table.0)];
    for (slot, record) in records {
        assert_eq!(slot, *end, "{table:?}: slot {slot} appended at {end}");
        changes.append(table, record.as_ref());
        *end += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table names = file layout (`zainod verify` and every existing directory read by them)
    #[test]
    fn schema_lays_out_heights_then_each_pools_levels_and_subtrees() {
        let schema = schema(NetworkType::Regtest);
        let tables: Vec<(&str, Width)> =
            schema.sequences.iter().map(|table| (table.name.as_str(), table.record)).collect();
        assert_eq!(tables.len(), 1 + 3 * 33);
        assert_eq!(tables[0], ("heights", Width::fixed(48)));
        for (pool, base) in [("sapling", 1), ("orchard", 34), ("ironwood", 67)] {
            assert_eq!(tables[base], (&*format!("{pool}/l00"), Width::fixed(32)));
            assert_eq!(tables[base + 31], (&*format!("{pool}/l31"), Width::fixed(32)));
            assert_eq!(tables[base + 32], (&*format!("{pool}/subtrees"), Width::fixed(36)));
        }
        let orchard_l05 = schema.sequence(level_table(ShieldedPool::Orchard, 5));
        assert_eq!(orchard_l05.name, "orchard/l05");
        assert_eq!(
            schema.sequence(subtree_table(ShieldedPool::Ironwood)).name,
            "ironwood/subtrees"
        );
    }
}
