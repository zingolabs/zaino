#![doc = include_str!("../usage.md")]

use zaino_persistence::{SequenceTable, Tables, Width};
use zcash_protocol::ShieldedPool;

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

/// Buffered heap that commits a bulk run (a `Finalized` block commits at once)
///
/// - every commit syncs each level's file (~50) → 8 MiB = commit-bound in sandblast
/// - 64 MiB = durable tip too far behind between commits
pub const WRITE_BUFFER: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(16 << 20).expect("16 MiB is non-zero");

/// Every table `cache_writes`: the fold reads the tip's sizes + frontier nodes off them
const HEIGHTS: SequenceTable =
    SequenceTable::new(0, "heights", Width::fixed(heights::RECORD as u32)).cache_writes();

/// Level sequences + subtrees, per pool
const POOL_TABLES: usize = MERKLE_DEPTH as usize + 1;

/// Activation order (= table order)
pub(crate) const POOLS: [ShieldedPool; 3] =
    [ShieldedPool::Sapling, ShieldedPool::Orchard, ShieldedPool::Ironwood];

/// `heights`, then each pool's tables ([`POOLS`] order)
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
    let subtrees = SequenceTable::new(first + MERKLE_DEPTH as u16, subtrees, entry).cache_writes();
    let mut tables = [subtrees; POOL_TABLES];
    let mut level = 0;
    while level < levels.len() {
        let node = Width::fixed(NODE as u32);
        tables[level] =
            SequenceTable::new(first + level as u16, levels[level], node).cache_writes();
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
