//! Height → commitment-tree state (Sapling, Orchard, Ironwood); subtree index → subtree root
//!
//! # Data structure: incremental Merkle tree, retained nodes in per-level flat arrays (`nodes.rs`)
//!
//! ```text
//! <dir>/
//!   MANIFEST                  committed count, tip hash, every file's seal = the commit point
//!   heights.idx               one record per height (slot = height)
//!   sapling/ orchard/ ironwood/
//!     l00.dat … l31.dat       one file per tree level, 32 B per node
//!     subtrees.dat            one entry per completed 2^16-leaf subtree (slot = subtree index)
//!
//! heights.idx record, 48 B:   hash 32 ‖ time u32 ‖ sapling u32 ‖ orchard u32 ‖ ironwood u32 (LE)
//! subtrees.dat entry, 36 B:   root 32 ‖ completing height u32 LE
//! level files:                l00 = every leaf (slot = index); l01..l31 = even index only
//!                             (slot = index / 2)
//! ```
//!
//! - textbook incremental tree = frontier only (latest state); full tree = 2N nodes
//! - kept here: every leaf + every even-index internal node = exactly where any past frontier's
//!   ommers come from (ommer = left sibling = even) → ≈48 B per commitment (32 leaf + ≈16 internal)
//! - positional: no keys stored, offset = slot × stride, one mmap read per node
//! - read-heavy (librustzcash asks 1:1 with `GetBlockRange`) → no replay per read, no hashing
//! - non-finalized tier = [`NonFinalizedTrees`] (same fold above the manifest, `imbl`, RAM only)
//!
//! # Lookup (`ReadView::treestate`)
//!
//! ```text
//! height h ──▶ heights.idx[h] ──▶ hash, time, size s per pool        (non-finalized record first)
//!                                     │ per pool, position p = s - 1
//!                                     ▼
//!              leaf     l00[p]
//!              ommers   each set bit ℓ of p: node (ℓ, (p >> ℓ) - 1)  (≤ 32 reads, no hashing,
//!                         ℓ = 0 → l00[p - 1]                          non-finalized nodes first)
//!                         ℓ ≥ 1 → lℓ[((p >> ℓ) - 1) / 2]
//!                                     │
//!                                     ▼
//!              frontier → `write_commitment_tree` → `Treestate`
//!
//! subtree i ──▶ subtrees.dat[i] ──▶ root, completing height ──▶ heights.idx ──▶ completing hash
//! ```
//!
//! Page format and commit protocol: `zaino_persistence::{pages, dir}`,
//! `docs/design/index-data-structures.md` §3

use std::{io, path::Path, sync::Arc};

use arc_swap::ArcSwap;
use zaino_persistence::{
    dir::IndexDir,
    fs::Fs,
    manifest::{self, BodyReader, Committed, Identity, IndexKind, ManifestError},
    pages::{CommittedFiles, PagedFile, Pages, Sealed},
    StoreError,
};
use zaino_primitives::types::{BlockRef, Height, PerPool, ShieldedPool};
use zcash_protocol::consensus::NetworkType;

mod fold;
mod heights;
mod index_writer;
mod nodes;
mod serve;
mod subtrees;
mod view;

pub use index_writer::TreeStateIndexWriter;
pub use serve::{ServeError, TreeStateService};
pub use view::{NonFinalizedTrees, ReadView};

use heights::{TreeStateHeight, RECORD};
use nodes::{level_file, NodeFiles, PoolNodes, MERKLE_DEPTH};
use subtrees::{SubtreeFile, Subtrees};
use view::NonFinalizedPool;

/// On-disk layout version (bumped on any change to the files or the manifest body)
const FORMAT: u16 = 1;

const HEIGHTS: &str = "heights.idx";
const SUBTREES: &str = "subtrees.dat";
const LEVELS: usize = MERKLE_DEPTH as usize;

#[derive(Debug, thiserror::Error)]
pub enum IndexWriterError {
    #[error(transparent)]
    Store(#[from] StoreError),

    /// Stored nodes will not rebuild a frontier of the recorded size (a fold bug)
    #[error("tree-state store is inconsistent at size {size}")]
    Inconsistent { size: u64 },

    /// Non-canonical field element off the wire
    #[error("block {height} carries an uncommittable note commitment")]
    Commitment { height: Height },
}

fn identity(network: NetworkType) -> Identity {
    Identity { kind: IndexKind::TreeState, format: FORMAT, network }
}

/// One pool's file seals
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PoolSeals {
    levels: [Sealed; LEVELS],
    subtrees: Sealed,
}

impl PoolSeals {
    const EMPTY: Self = Self { levels: [Sealed::EMPTY; LEVELS], subtrees: Sealed::EMPTY };
}

/// Committed state, as the manifest body stores it
#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    committed: Committed,
    heights: Sealed,
    pools: PerPool<PoolSeals>,
}

impl Body {
    const EMPTY: Self = Self {
        committed: Committed::EMPTY,
        heights: Sealed::EMPTY,
        pools: PerPool {
            sapling: PoolSeals::EMPTY,
            orchard: PoolSeals::EMPTY,
            ironwood: PoolSeals::EMPTY,
        },
    };

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.committed.encode(&mut out);
        self.heights.encode(&mut out);
        for pool in ShieldedPool::ALL {
            let seals = self.pools.get(pool);
            for sealed in seals.levels.iter().chain([&seals.subtrees]) {
                sealed.encode(&mut out);
            }
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        let mut body = BodyReader::new(bytes);
        let committed = Committed::decode(&mut body)?;
        let heights = Sealed::decode(&mut body)?;
        let pools = PerPool::try_from_fn(|_| {
            let mut seals = PoolSeals::EMPTY;
            for level in &mut seals.levels {
                *level = Sealed::decode(&mut body)?;
            }
            seals.subtrees = Sealed::decode(&mut body)?;
            Ok::<_, ManifestError>(seals)
        })?;
        body.finish()?;
        if heights.len != committed.count() * RECORD as u64 {
            return Err(ManifestError::Body("heights.idx disagrees with the committed count"));
        }
        Ok(Self { committed, heights, pools })
    }
}

/// Every file `dir`'s manifest seals (offline scrub; plain reads, no lock)
pub fn committed_files(dir: &Path, network: NetworkType) -> io::Result<CommittedFiles> {
    let body = match manifest::read(dir, identity(network))? {
        Some(bytes) => Body::decode(&bytes).map_err(io::Error::other)?,
        None => Body::EMPTY,
    };
    let mut files = vec![(HEIGHTS.to_owned(), body.heights)];
    for pool in ShieldedPool::ALL {
        let seals = body.pools.get(pool);
        for (level, sealed) in (0u8..).zip(seals.levels) {
            files.push((format!("{pool}/{}", level_file(level)), sealed));
        }
        files.push((format!("{pool}/{SUBTREES}"), seals.subtrees));
    }
    Ok(CommittedFiles { tip: body.committed.height(), files })
}

/// One pool's published read view
#[derive(Debug)]
pub(crate) struct PoolView {
    nodes: PoolNodes,
    subtrees: Subtrees,
}

/// Committed state, all three pools, republished on every commit
#[derive(Debug)]
pub(crate) struct Snapshot {
    heights: Pages,
    /// Last committed height, inclusive (`None` = nothing committed)
    tip: Option<Height>,
    pools: PerPool<PoolView>,
}

impl Snapshot {
    /// `None` above the committed tip
    fn height(&self, height: Height) -> Option<TreeStateHeight> {
        if Some(height) > self.tip {
            return None;
        }
        let at = usize::try_from(u32::from(height)).expect("heights fit usize") * RECORD;
        Some(heights::decode(self.heights.read(at..at + RECORD).try_into().expect("RECORD bytes")))
    }
}

/// One pool's files
#[derive(Debug)]
struct PoolFiles {
    nodes: NodeFiles,
    subtrees: SubtreeFile,
}

impl PoolFiles {
    /// Opens `<index>/<pool>/` at `seals`
    fn open(
        fs: &dyn Fs,
        dir: &IndexDir,
        pool: ShieldedPool,
        seals: &PoolSeals,
    ) -> Result<Self, StoreError> {
        let path = dir.subdir(&pool.to_string())?;
        let mut levels = Vec::with_capacity(LEVELS);
        for (level, sealed) in (0u8..).zip(seals.levels) {
            levels.push(PagedFile::open(fs, &path.join(level_file(level)), sealed)?);
        }
        let subtrees = PagedFile::open(fs, &path.join(SUBTREES), seals.subtrees)?;
        fs.sync_dir(&path)?;

        Ok(Self {
            nodes: NodeFiles::new(levels, seals.levels),
            subtrees: SubtreeFile::new(subtrees, seals.subtrees),
        })
    }

    /// Appends `chunk`; durable only once [`seal`](Self::seal) returns
    fn write(&mut self, chunk: &NonFinalizedPool) -> Result<(), StoreError> {
        for (&slot, node) in &chunk.nodes {
            self.nodes.put(slot, node)?;
        }
        for (&index, root) in &chunk.subtrees {
            self.subtrees.put(index, root)?;
        }
        Ok(())
    }

    fn seal(&mut self) -> Result<PoolSeals, StoreError> {
        Ok(PoolSeals { levels: self.nodes.seal()?, subtrees: self.subtrees.seal()? })
    }

    /// Remaps at the last seals, keeping `previous`'s checked pages
    fn snapshot(&self, previous: Option<&PoolView>) -> Result<PoolView, StoreError> {
        Ok(PoolView {
            nodes: self.nodes.snapshot(previous.map(|old| &old.nodes))?,
            subtrees: self.subtrees.snapshot(previous.map(|old| &old.subtrees))?,
        })
    }
}

/// Append-only, single-writer store (lands already-folded non-finalized chunks)
#[derive(Debug)]
pub struct TreeStateStore {
    dir: IndexDir,
    heights: PagedFile,
    pools: PerPool<PoolFiles>,
    snapshot: Arc<ArcSwap<Snapshot>>,
    committed: Body,
}

impl TreeStateStore {
    /// Opens `path` at its committed state (lengths and tail pages checked); fresh = empty
    pub fn open(fs: Arc<dyn Fs>, path: &Path, network: NetworkType) -> Result<Self, StoreError> {
        let opened = IndexDir::open(Arc::clone(&fs), path, identity(network))?;
        let dir = opened.dir;
        let committed = match &opened.body {
            Some(body) => Body::decode(body)?,
            None => {
                dir.ensure_empty(HEIGHTS)?;
                dir.ensure_empty(&format!("{HEIGHTS}.crc"))?;
                for pool in ShieldedPool::ALL {
                    dir.ensure_empty_dir(&pool.to_string())?;
                }
                Body::EMPTY
            }
        };

        let heights = PagedFile::open(fs.as_ref(), &dir.path().join(HEIGHTS), committed.heights)?;
        let pools = PerPool::try_from_fn(|pool| {
            PoolFiles::open(fs.as_ref(), &dir, pool, committed.pools.get(pool))
        })?;
        if opened.body.is_none() {
            dir.commit(&committed.encode())?;
        }

        let snapshot = Snapshot {
            heights: heights.pages(committed.heights, None)?,
            tip: committed.committed.height(),
            pools: PerPool::try_from_fn(|pool| pools.get(pool).snapshot(None))?,
        };
        Ok(Self {
            dir,
            heights,
            pools,
            snapshot: Arc::new(ArcSwap::from_pointee(snapshot)),
            committed,
        })
    }

    /// Last committed block, inclusive (`None` = nothing committed)
    pub(crate) fn finalized_tip(&self) -> Option<BlockRef> {
        self.committed.committed.tip
    }

    /// [`finalized_tip`](Self::finalized_tip)'s height
    pub(crate) fn finalized_height(&self) -> Option<Height> {
        self.committed.committed.height()
    }

    /// Committed state as published (what readers pin)
    pub(crate) fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    /// Appends `chunk` (heights, nodes, subtree roots), seals every file, commits
    pub(crate) fn write(&mut self, chunk: &NonFinalizedTrees) -> Result<(), StoreError> {
        let (Some(&(start, _)), Some(&(last_height, last))) =
            (chunk.heights.get_min(), chunk.heights.get_max())
        else {
            return Ok(());
        };
        let next = self.finalized_height().map_or(Height::GENESIS, Height::next);
        assert_eq!(start, next, "chunk off the committed tip");
        assert_eq!(chunk.tip, Some(last_height), "chunk ends at its last record");

        for record in chunk.heights.values() {
            self.heights.append(&heights::encode(record))?;
        }
        for pool in ShieldedPool::ALL {
            self.pools.get_mut(pool).write(chunk.pools.get(pool))?;
        }

        let body = Body {
            committed: Committed { tip: Some(BlockRef { hash: last.hash, height: last_height }) },
            heights: self.heights.seal()?,
            pools: PerPool::try_from_fn(|pool| self.pools.get_mut(pool).seal())?,
        };
        self.dir.commit(&body.encode())?;
        self.committed = body;

        self.publish()
    }

    /// Remaps and publishes the committed state (pages already checked stay checked)
    fn publish(&mut self) -> Result<(), StoreError> {
        let old = self.snapshot.load();
        let pools =
            PerPool::try_from_fn(|pool| self.pools.get(pool).snapshot(Some(old.pools.get(pool))))?;
        self.snapshot.store(Arc::new(Snapshot {
            heights: self.heights.pages(self.committed.heights, Some(&old.heights))?,
            tip: self.finalized_height(),
            pools,
        }));
        Ok(())
    }
}
