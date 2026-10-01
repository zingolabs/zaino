//! Block hash → height
//!
//! # Data structure: size-tiered LSM-Tree of immutable sorted segments (`zaino_persistence::lsm`)
//!
//! ```text
//! <dir>/
//!   MANIFEST           committed count, tip hash, segment list (id, rows, seal) = the commit point
//!   by_hash/<id>.seg   one immutable sorted segment (+ `.crc`: page checksums)
//!
//! one segment file:
//!   rows      hash ‖ height u32 BE, 36 B each, sorted by hash, packed
//!   fences    first hash of every 4 KiB block of rows (≈113 rows per block)
//!   filter    binary fuse, 8-bit fingerprints: ≈9 bits per hash.  uses block-hash directly, no hashing
//! ```
//!
//! - LSM minus everything mutable data needs: a finalised block's height never changes → no
//!   memtable, no WAL, no tombstones, no versions, no dedup on merge
//! - memtable role = [`ReadView`]'s non-finalized map (reorgable blocks, RAM only, never persisted)
//!
//! # Lookup (`ReadView::height_of_hash`)
//!
//! ```text
//! non-finalized map ──hit──▶ height
//!    │ miss
//!    ▼
//! each segment:  filter ──"absent"──▶ next segment      (every segment but ≤ 1, for a real hash)
//!                  │ "maybe"
//!                  ▼
//!                fences → one 4 KiB block → binary search ──found──▶ height
//!                                                  └──not found──▶ next segment (1/256 case)
//! ```
//!
//! Policy and file format: `zaino_persistence::lsm`, `docs/design/index-data-structures.md`

use std::{io, path::Path, sync::Arc};

use zaino_persistence::{
    fs::Fs,
    lsm::{self, LsmIndex, LsmStore, SegmentLog, SegmentMeta, SegmentSet},
    manifest::{Committed, IndexKind, ManifestError},
    pages::CommittedFiles,
    StoreError,
};
use zaino_primitives::types::{BlockHash, BlockRef, Height};
use zcash_protocol::consensus::NetworkType;

use by_hash::{HashKey, HashRow};

mod by_hash;
mod index_writer;
mod serve;
mod view;

pub use index_writer::BlockHashIndexWriter;
pub use serve::{BlockHashService, ServeError};
pub use view::ReadView;

/// Block hash width
pub(crate) const HASH: usize = 32;

/// Every file `dir`'s manifest seals (offline scrub; plain reads, no lock)
pub fn committed_files(dir: &Path, network: NetworkType) -> io::Result<CommittedFiles> {
    lsm::committed_files::<BlockHashIndex>(dir, network)
}

/// On disk: one `by_hash` segment set, one row per committed height
struct BlockHashIndex;

impl LsmIndex for BlockHashIndex {
    const KIND: IndexKind = IndexKind::BlockHash;
    const FORMAT: u16 = 1;
    const SETS: &'static [&'static str] = &["by_hash"];
    type Logs = SegmentLog<HashRow>;

    fn check(committed: &Committed, lists: &[Vec<SegmentMeta>]) -> Result<(), ManifestError> {
        let rows: u64 = lists.iter().flatten().map(|segment| segment.records).sum();
        if rows != committed.count() {
            return Err(ManifestError::Body("segment rows disagree with the committed count"));
        }
        Ok(())
    }
}

/// Single-writer store of finalised hashes
pub struct BlockHashStore {
    segments: LsmStore<BlockHashIndex>,
}

/// Cloneable read handle onto the committed segments
#[derive(Debug, Clone)]
pub struct BlockHashReader {
    segments: SegmentSet<HashKey>,
}

impl BlockHashReader {
    /// Committed segments alone, no non-finalized blocks
    pub fn pin(&self) -> ReadView {
        ReadView::new(imbl::HashMap::new(), self.segments.pin())
    }

    /// Committed segments as published now
    pub(crate) fn pin_segments(&self) -> Arc<lsm::Snapshot<HashKey>> {
        self.segments.pin()
    }
}

impl BlockHashStore {
    /// Opens `path` at its committed state (unlisted segments removed); fresh = empty
    pub fn open(fs: Arc<dyn Fs>, path: &Path, network: NetworkType) -> Result<Self, StoreError> {
        Ok(Self { segments: LsmStore::open(fs, path, network)? })
    }

    pub fn reader(&self) -> BlockHashReader {
        BlockHashReader { segments: self.segments.sets() }
    }

    /// Last committed block, inclusive (`None` = nothing committed)
    pub(crate) fn finalized_tip(&self) -> Option<BlockRef> {
        self.segments.committed().tip
    }

    pub(crate) fn path(&self) -> &Path {
        self.segments.path()
    }

    /// Makes `blocks` durable and visible as one new segment
    ///
    /// - `blocks` contiguous from the height after the committed tip (asserted)
    pub fn commit(&mut self, blocks: &[(Height, [u8; HASH])]) -> Result<(), StoreError> {
        let Some(&(height, hash)) = blocks.last() else {
            return Ok(());
        };
        let mut reached = self.finalized_tip().map(|tip| tip.height);
        let rows = blocks
            .iter()
            .map(|&(height, hash)| {
                let next = reached.map_or(Height::GENESIS, Height::next);
                assert_eq!(height, next, "block-hash commit out of order");
                reached = Some(height);
                HashRow { hash: HashKey(hash), height: u32::from(height) }
            })
            .collect();
        self.segments.commit(rows, BlockRef { hash: BlockHash::from(hash), height })
    }
}

#[cfg(test)]
mod tests {
    use zaino_persistence::fs::SimFs;

    use super::*;

    const NET: NetworkType = NetworkType::Regtest;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    fn hash(height: Height) -> [u8; HASH] {
        let mut out = [0u8; HASH];
        out[..4].copy_from_slice(&u32::from(height).to_le_bytes());
        out
    }

    /// Heights `start` to `end`, both inclusive, each with its hash
    fn blocks(start: u32, end: u32) -> Vec<(Height, [u8; HASH])> {
        (start..=end).map(|n| (h(n), hash(h(n)))).collect()
    }

    /// Three commits crashed after every operation: each state reopens to an acknowledged or
    /// attempted commit, locates every hash it holds, and accepts the next commit
    #[test]
    fn every_crash_state_reopens_to_a_committed_prefix_that_keeps_committing() {
        let fs = SimFs::recording();
        let path = Path::new("/bh");
        let commits = [(0, 2), (3, 4), (5, 5)];
        {
            let mut store = BlockHashStore::open(fs.clone(), path, NET).expect("open");
            for (acked, (first, last)) in (1u64..).zip(commits) {
                store.commit(&blocks(first, last)).expect("commit");
                fs.set_tag(acked);
            }
        }
        // last committed height (inclusive) once `commits_done` commits landed
        let tip_after = |commits_done: u64| match commits_done {
            0 => None,
            1 => Some(h(2)),
            2 => Some(h(4)),
            _ => Some(h(5)),
        };

        let states = fs.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let mut store = BlockHashStore::open(state.fs, path, NET)
                .unwrap_or_else(|error| panic!("{label}: {error}"));
            let tip = store.finalized_tip().map(|tip| tip.height);
            let acked = [tip_after(state.tag), tip_after(state.tag + 1)];
            assert!(acked.contains(&tip), "{label}: recovered through {tip:?}");
            let view = store.reader().pin();
            for height in tip.into_iter().flat_map(|last| h(0).up_to(last)) {
                assert_eq!(view.height_of_hash(&hash(height)), Some(height), "{label}: {height}");
            }
            let expected =
                tip.map(|height| BlockRef { hash: BlockHash::from(hash(height)), height });
            assert_eq!(store.finalized_tip(), expected, "{label}");
            let next = tip.map_or(Height::GENESIS, Height::next);
            assert_eq!(view.height_of_hash(&hash(next)), None, "{label}: past the end");

            store.commit(&[(next, hash(next))]).expect("commit after recovery");
            let located = store.reader().pin().height_of_hash(&hash(next));
            assert_eq!(located, Some(next), "{label}: commits continue at the recovered end");
        }
    }

    /// Locates across many commits (forcing merges) and a reopen; twins sharing a filter shard
    /// (first 8 bytes) resolve apart; a view pinned before a commit never sees it
    #[test]
    fn locates_across_merges_and_a_reopen() {
        let fs = SimFs::new();
        let path = Path::new("/bh");
        let mut store = BlockHashStore::open(fs.clone(), path, NET).expect("open");
        assert_eq!(store.reader().pin().height_of_hash(&hash(h(0))), None);

        // 20 single-block commits: tier-0 segments, merged 8 at a time in the background
        for n in 0..20 {
            store.commit(&blocks(n, n)).expect("commit");
        }
        let before = store.reader().pin();

        let mut twin_a = [9u8; HASH];
        let mut twin_b = [9u8; HASH];
        twin_a[31] = 0xaa;
        twin_b[31] = 0xbb;
        store.segments.logs().settle();
        store.commit(&[(h(20), twin_a), (h(21), twin_b)]).expect("commit twins");
        let segments = store.segments.logs().segments().len();
        assert!(segments < 21, "21 commits, merged into {segments} segments");

        let lookup =
            |store: &BlockHashStore, hash: &[u8; HASH]| store.reader().pin().height_of_hash(hash);
        let committed: Vec<_> = (0..20).map(|n| lookup(&store, &hash(h(n)))).collect();
        let expected: Vec<_> = (0..20).map(|n| Some(h(n))).collect();
        assert_eq!(committed, expected, "every committed hash, whichever segment it merged into");
        let twins = (lookup(&store, &twin_a), lookup(&store, &twin_b));
        assert_eq!(twins, (Some(h(20)), Some(h(21))), "twins resolved apart");
        assert_eq!(lookup(&store, &[0xff; HASH]), None, "unknown hash");
        assert_eq!(before.height_of_hash(&twin_a), None, "pinned view keeps its segments");

        drop((store, before));
        let store = BlockHashStore::open(fs, path, NET).expect("reopen");
        let reopened = (lookup(&store, &twin_b), lookup(&store, &hash(h(7))));
        assert_eq!(store.finalized_tip().map(|tip| tip.height), Some(h(21)));
        assert_eq!(reopened, (Some(h(21)), Some(h(7))));
    }
}
