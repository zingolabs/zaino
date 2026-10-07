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
//! - one block = its height record + the nodes and subtree roots it completes ([`fold`]);
//!   non-final blocks = `zaino-nfs` layers (same positions, RAM only)
//!
//! # Lookup ([`TreeStateReader`]: a request's tree state, a fold's parent frontier)
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

use zaino_persistence::{SequenceTable, Tables, Width};
use zaino_primitives::types::ShieldedPool;

mod heights;
mod nodes;
mod reader;
mod serve;
mod subtrees;
mod writer;

pub use reader::TreeStateReader;
pub use serve::{PoolActivations, ServeError};
pub use writer::{fold, FoldError, TreeStateIndexWriter};

use nodes::{MERKLE_DEPTH, NODE};

/// On-disk layout version
pub const FORMAT: u16 = 1;

/// What the store holds (`zainod` opens and verifies it by these)
pub const TABLES: Tables = Tables::new(&SEQUENCES, &[]);

const HEIGHTS: SequenceTable =
    SequenceTable::new(0, "heights", Width::fixed(heights::RECORD as u32));

/// Level sequences + subtrees, per pool
const POOL_TABLES: usize = MERKLE_DEPTH as usize + 1;

/// `heights`, then each pool's tables (`ShieldedPool::ALL` order)
const SEQUENCES: [SequenceTable; 1 + 3 * POOL_TABLES] = {
    let mut all = [HEIGHTS; 1 + 3 * POOL_TABLES];
    let mut at = 0;
    while at < POOL_TABLES {
        all[1 + at] = SAPLING[at];
        all[1 + POOL_TABLES + at] = ORCHARD[at];
        all[1 + 2 * POOL_TABLES + at] = IRONWOOD[at];
        at += 1;
    }
    all
};

/// `"<pool>/l00"` … `"<pool>/l31"` (`concat!`: names = `&'static str` in a `const`)
macro_rules! level_names {
    ($pool:literal) => {
        level_names!($pool; l00 l01 l02 l03 l04 l05 l06 l07 l08 l09 l10 l11 l12 l13 l14 l15
            l16 l17 l18 l19 l20 l21 l22 l23 l24 l25 l26 l27 l28 l29 l30 l31)
    };
    ($pool:literal; $($level:ident)*) => {
        [$(concat!($pool, "/", stringify!($level))),*]
    };
}

const SAPLING: [SequenceTable; POOL_TABLES] =
    pool_tables(1, level_names!("sapling"), "sapling/subtrees");
const ORCHARD: [SequenceTable; POOL_TABLES] =
    pool_tables(1 + POOL_TABLES as u16, level_names!("orchard"), "orchard/subtrees");
const IRONWOOD: [SequenceTable; POOL_TABLES] =
    pool_tables(1 + 2 * POOL_TABLES as u16, level_names!("ironwood"), "ironwood/subtrees");

/// One pool's tables from id `first`: level ℓ's retained nodes (slot = [`nodes::slot`]), then
/// its completed subtrees (slot = subtree index)
const fn pool_tables(
    first: u16,
    levels: [&'static str; MERKLE_DEPTH as usize],
    subtrees: &'static str,
) -> [SequenceTable; POOL_TABLES] {
    let entry = Width::fixed(subtrees::ENTRY as u32);
    let mut tables =
        [SequenceTable::new(first + MERKLE_DEPTH as u16, subtrees, entry); POOL_TABLES];
    let mut level = 0;
    while level < levels.len() {
        tables[level] =
            SequenceTable::new(first + level as u16, levels[level], Width::fixed(NODE as u32));
        level += 1;
    }
    tables
}

fn pool_tables_of(pool: ShieldedPool) -> &'static [SequenceTable; POOL_TABLES] {
    match pool {
        ShieldedPool::Sapling => &SAPLING,
        ShieldedPool::Orchard => &ORCHARD,
        ShieldedPool::Ironwood => &IRONWOOD,
    }
}

/// `<pool>/l{level:02}`: retained nodes of `level`, slot = [`nodes::slot`]
pub(crate) fn level_table(pool: ShieldedPool, level: u8) -> SequenceTable {
    pool_tables_of(pool)[usize::from(level)]
}

/// `<pool>/subtrees`: slot = subtree index
pub(crate) fn subtree_table(pool: ShieldedPool) -> SequenceTable {
    pool_tables_of(pool)[usize::from(MERKLE_DEPTH)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table names = file layout (`zainod verify` and every existing directory read by them)
    #[test]
    fn tables_lay_out_heights_then_each_pools_levels_and_subtrees() {
        let tables: Vec<(&str, Width)> =
            SEQUENCES.iter().map(|table| (table.name, table.record)).collect();
        assert_eq!(tables.len(), 1 + 3 * 33);
        assert_eq!(tables[0], ("heights", Width::fixed(48)));
        for (pool, base) in [("sapling", 1), ("orchard", 34), ("ironwood", 67)] {
            assert_eq!(tables[base], (&*format!("{pool}/l00"), Width::fixed(32)));
            assert_eq!(tables[base + 9], (&*format!("{pool}/l09"), Width::fixed(32)));
            assert_eq!(tables[base + 31], (&*format!("{pool}/l31"), Width::fixed(32)));
            assert_eq!(tables[base + 32], (&*format!("{pool}/subtrees"), Width::fixed(36)));
        }
        assert_eq!(level_table(ShieldedPool::Orchard, 5), SEQUENCES[34 + 5]);
        assert_eq!(level_table(ShieldedPool::Orchard, 5).name, "orchard/l05");
        assert_eq!(subtree_table(ShieldedPool::Ironwood).name, "ironwood/subtrees");
    }
}
