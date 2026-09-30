//! Height → gRPC-framed `CompactBlock` record; height range → one contiguous byte span
//!
//! # Data structure: append-only heap file + dense positional offset index (`record.rs`)
//!
//! ```text
//! <dir>/
//!   MANIFEST      committed count, tip hash, tip tree sizes, each file's seal = the commit point
//!   blocks.dat    framed records, back to back, height order
//!   offsets.idx   8 B per height (slot = height): where its record ends in blocks.dat
//!
//! one record:     [0x00][len u32 BE][CompactBlock protobuf] = the exact bytes on the wire
//!                 every pool included (pruned on read, `project.rs`)
//! offsets.idx:    end u64 LE at slot h; start = end at h - 1 (0 for genesis)
//! ```
//!
//! - no keys stored: offset = h × 8, one read per bound
//! - records = wire bytes: serving = byte movement (no decode, no re-encode)
//! - non-finalized tier = `NonFinalizedState` (applied records, same bytes, `imbl`, RAM only)
//! - hash → height = `zaino-internal-block-hash-to-height` (record's own `hash` confirms a hit)
//!
//! # Lookup (`ReadView::block`, `ReadView::span_from`)
//!
//! ```text
//! height h ──▶ non-finalized map ──hit──▶ record
//!    │ miss
//!    ▼
//! offsets.idx[h - 1], offsets.idx[h] ──▶ blocks.dat[start..end]   (a slice of the mapping)
//!
//! heights start..=end ──▶ offsets.idx[start - 1] .. offsets.idx[k] ──▶ one contiguous span
//!                         (k = last record inside the window budget; next window starts at k + 1)
//! ```
//!
//! - Byte offsets: `start` inclusive, `end` exclusive
//! - Heights: `start` to `end`, both inclusive
//!
//! Page format and commit protocol: `zaino_persistence::{pages, dir}`,
//! `docs/design/index-data-structures.md` §3

use std::{io, ops::Range, path::Path, sync::Arc};

use arc_swap::ArcSwap;
use bytes::Bytes;
use zaino_persistence::{
    dir::IndexDir,
    fs::Fs,
    manifest::{self, BodyReader, Committed, Identity, IndexKind, ManifestError},
    pages::{CommittedFiles, PagedFile, Pages, Sealed},
    StoreError,
};
use zaino_primitives::types::{BlockHash, BlockRef, Height, TreeSize, TreeSizes};
use zcash_protocol::consensus::NetworkType;

mod build;
mod index_writer;
mod non_finalized;
mod project;
mod record;
mod serve;
mod view;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use build::{compact_tx, encode_compact_block};
pub use index_writer::{CompactBlockIndexWriter, IndexWriterError};
pub(crate) use non_finalized::NonFinalizedState;
pub use project::Pools;
pub(crate) use record::{HASH, OFFSET};
pub use serve::{CompactBlockService, RangeCursor, ServeError};
pub use view::ReadView;

/// On-disk layout version (bumped on any change to the files, the records or the manifest body)
const FORMAT: u16 = 1;

const BLOCKS: &str = "blocks.dat";
const OFFSETS: &str = "offsets.idx";

type Result<T> = std::result::Result<T, StoreError>;

fn identity(network: NetworkType) -> Identity {
    Identity { kind: IndexKind::CompactBlock, format: FORMAT, network }
}

/// Committed state, as the manifest body stores it
#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    committed: Committed,
    sizes: TreeSizes,
    blocks: Sealed,
    offsets: Sealed,
}

impl Body {
    const EMPTY: Self = Self {
        committed: Committed::EMPTY,
        sizes: TreeSizes::ZERO,
        blocks: Sealed::EMPTY,
        offsets: Sealed::EMPTY,
    };

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.committed.encode(&mut out);
        for size in [self.sizes.sapling, self.sizes.orchard, self.sizes.ironwood] {
            out.extend_from_slice(&u32::from(size).to_le_bytes());
        }
        for sealed in [self.blocks, self.offsets] {
            sealed.encode(&mut out);
        }
        out
    }

    fn decode(bytes: &[u8]) -> std::result::Result<Self, ManifestError> {
        let mut body = BodyReader::new(bytes);
        let committed = Committed::decode(&mut body)?;
        let mut size = || body.u32().map(TreeSize::from);
        let sizes = TreeSizes { sapling: size()?, orchard: size()?, ironwood: size()? };
        let blocks = Sealed::decode(&mut body)?;
        let offsets = Sealed::decode(&mut body)?;
        body.finish()?;

        if offsets.len != committed.count() * OFFSET as u64 {
            return Err(ManifestError::Body("offsets.idx disagrees with the committed count"));
        }
        Ok(Self { committed, sizes, blocks, offsets })
    }
}

/// Every file `dir`'s manifest seals (offline scrub; plain reads, no lock)
pub fn committed_files(dir: &Path, network: NetworkType) -> io::Result<CommittedFiles> {
    let body = match manifest::read(dir, identity(network))? {
        Some(bytes) => Body::decode(&bytes).map_err(io::Error::other)?,
        None => Body::EMPTY,
    };
    Ok(CommittedFiles {
        tip: body.committed.height(),
        files: vec![(BLOCKS.to_owned(), body.blocks), (OFFSETS.to_owned(), body.offsets)],
    })
}

/// A consistent read view of the committed files; `tip` = last committed height, inclusive
/// (`None` = empty)
#[derive(Debug)]
pub(crate) struct Snapshot {
    blocks: Pages,
    offsets: Pages,
    tip: Option<Height>,
}

/// File index of a committed height
fn slot(height: Height) -> usize {
    usize::try_from(u32::from(height)).expect("heights fit usize")
}

impl Snapshot {
    /// Byte range of a committed height's record in `blocks.dat` (`start` inclusive, `end`
    /// exclusive)
    fn record(&self, height: Height) -> Option<Range<usize>> {
        if Some(height) > self.tip {
            return None;
        }
        let end_of = |height: usize| {
            let at = height * OFFSET;
            let end = u64::from_le_bytes(
                self.offsets.read(at..at + OFFSET).try_into().expect("OFFSET bytes"),
            );
            usize::try_from(end).expect("committed offsets fit usize")
        };
        let height = slot(height);
        let start = height.checked_sub(1).map_or(0, end_of);
        Some(start..end_of(height))
    }

    /// One record, framed and wire-ready (a slice of the mapping, no copy)
    pub(crate) fn block(&self, height: Height) -> Option<Bytes> {
        Some(self.blocks.bytes(self.record(height)?))
    }

    /// Record-aligned prefix of heights `start` to `end`, both inclusive, plus the last height it
    /// reaches (inclusive)
    ///
    /// - window bounded by `budget`, not by the range (unbounded range != unbounded work)
    /// - always >= 1 record, so a caller looping on the reach makes progress
    /// - contiguous + framed, so callers slice it per record with refcounted [`Bytes`]
    /// - a **slice** of the mapping: the window costs no allocation and no copy
    pub(crate) fn span_from(
        &self,
        start: Height,
        end: Height,
        budget: usize,
    ) -> Option<(Bytes, Height)> {
        assert!(start <= end, "span {start}..={end} reversed");
        let Range { start: byte_start, end: mut byte_end } = self.record(start)?;
        let mut last = start;

        for next in start.next().up_to(end) {
            let Some(record) = self.record(next) else {
                break;
            };
            if record.end - byte_start > budget {
                break;
            }
            byte_end = record.end;
            last = next;
        }

        // one readahead per window: faults here on the blocking step, not on a runtime worker
        // (4.3x cold, docs/design/persistence-architecture.md)
        self.blocks.will_need(byte_start..byte_end);
        Some((self.blocks.bytes(byte_start..byte_end), last))
    }
}

/// Append-only, single-writer store
#[derive(Debug)]
pub struct CompactBlockStore {
    dir: IndexDir,
    blocks: PagedFile,
    offsets: PagedFile,
    snapshot: Arc<ArcSwap<Snapshot>>,
    committed: Body,
    /// Last appended block, inclusive (= the committed tip until an append; `None` = empty)
    appended: Option<BlockRef>,
}

/// Cloneable read handle onto the committed records
#[derive(Debug, Clone)]
pub struct CompactBlockReader {
    snapshot: Arc<ArcSwap<Snapshot>>,
}

impl CompactBlockReader {
    /// Committed records alone, no non-finalized tier above them
    pub fn pin(&self) -> ReadView {
        self.pin_with(NonFinalizedState::default())
    }

    /// Current publication, `non_finalized` above it
    pub(crate) fn pin_with(&self, non_finalized: NonFinalizedState) -> ReadView {
        ReadView::new(non_finalized, self.snapshot())
    }

    /// Committed records as published now
    pub(crate) fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }
}

impl CompactBlockStore {
    /// Opens `path` at its committed state (lengths and tail pages checked); fresh = empty
    pub fn open(fs: Arc<dyn Fs>, path: &Path, network: NetworkType) -> Result<Self> {
        let opened = IndexDir::open(Arc::clone(&fs), path, identity(network))?;
        let dir = opened.dir;
        let committed = match &opened.body {
            Some(body) => Body::decode(body)?,
            None => {
                for file in [BLOCKS, OFFSETS] {
                    dir.ensure_empty(file)?;
                    dir.ensure_empty(&format!("{file}.crc"))?;
                }
                Body::EMPTY
            }
        };

        let file =
            |name: &str, sealed| PagedFile::open(fs.as_ref(), &dir.path().join(name), sealed);
        let blocks = file(BLOCKS, committed.blocks)?;
        let offsets = file(OFFSETS, committed.offsets)?;
        if opened.body.is_none() {
            dir.commit(&committed.encode())?;
        }

        let snapshot = Snapshot {
            blocks: blocks.pages(committed.blocks, None)?,
            offsets: offsets.pages(committed.offsets, None)?,
            tip: committed.committed.height(),
        };
        Ok(Self {
            dir,
            blocks,
            offsets,
            snapshot: Arc::new(ArcSwap::from_pointee(snapshot)),
            appended: committed.committed.tip,
            committed,
        })
    }

    /// Independent of the writer's lifetime
    pub fn reader(&self) -> CompactBlockReader {
        CompactBlockReader { snapshot: Arc::clone(&self.snapshot) }
    }

    /// Last appended height, inclusive, committed or not (`None` = empty; [`append`](Self::append)
    /// continues at the height after it)
    pub fn appended(&self) -> Option<Height> {
        self.appended.map(|tip| tip.height)
    }

    /// Last committed block, inclusive (what readers can be served; `None` = empty)
    pub fn finalized_tip(&self) -> Option<BlockRef> {
        self.committed.committed.tip
    }

    /// [`finalized_tip`](Self::finalized_tip)'s height
    pub fn finalized_height(&self) -> Option<Height> {
        self.committed.committed.height()
    }

    /// Tree sizes after the committed tip, as committed with it
    pub fn sizes(&self) -> TreeSizes {
        self.committed.sizes
    }

    /// Appends one record, stored as given ([`encode_compact_block`]'s framed bytes)
    ///
    /// - neither durable nor visible until [`commit`](Self::commit)
    /// - heights address the files directly: sequential, asserted
    pub fn append(&mut self, height: Height, hash: [u8; HASH], framed: &[u8]) -> Result<()> {
        let next = self.appended().map_or(Height::GENESIS, Height::next);
        assert_eq!(height, next, "compact store append out of order");
        assert_eq!(
            record::framed_len(framed),
            Some(framed.len()),
            "{height}: not one framed record"
        );

        self.blocks.append(framed)?;
        self.offsets.append(&self.blocks.len().to_le_bytes())?;
        self.appended = Some(BlockRef { hash: BlockHash::from(hash), height });
        Ok(())
    }

    /// Makes every appended record durable and visible; `sizes` = tree sizes after the last one
    ///
    /// - files sealed (fsync) → manifest → records published
    /// - nothing appended since the last commit = no-op
    pub fn commit(&mut self, sizes: TreeSizes) -> Result<()> {
        if self.appended == self.committed.committed.tip {
            return Ok(());
        }
        let body = Body {
            committed: Committed { tip: self.appended },
            sizes,
            blocks: self.blocks.seal()?,
            offsets: self.offsets.seal()?,
        };
        self.dir.commit(&body.encode())?;
        self.committed = body;
        self.publish()
    }

    /// Remaps and publishes the committed state (pages already checked stay checked)
    fn publish(&mut self) -> Result<()> {
        let old = self.snapshot.load();
        let body = &self.committed;
        self.snapshot.store(Arc::new(Snapshot {
            blocks: self.blocks.pages(body.blocks, Some(&old.blocks))?,
            offsets: self.offsets.pages(body.offsets, Some(&old.offsets))?,
            tip: body.committed.height(),
        }));
        Ok(())
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

    fn hash(height: Height) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[..4].copy_from_slice(&u32::from(height).to_le_bytes());
        out
    }

    fn sizes(height: Height) -> TreeSizes {
        TreeSizes {
            sapling: TreeSize::from(u32::from(height)),
            orchard: TreeSize::from(0),
            ironwood: TreeSize::from(1),
        }
    }

    /// Record of a height-dependent length (`blocks.dat` offsets differ per record)
    fn framed(height: Height) -> Vec<u8> {
        let n = u32::from(height);
        let mut out = Vec::new();
        record::frame_into(&mut out, |out| {
            out.resize(record::FRAME_HEADER + 8 + n as usize % 17, n as u8)
        });
        out
    }

    #[test]
    fn appends_reopen_and_serve_single_and_range_reads() {
        let fs = SimFs::new();
        let path = Path::new("/cb");

        let mut store = CompactBlockStore::open(fs.clone(), path, NET).expect("open");
        assert_eq!(store.finalized_tip(), None);
        assert!(store.reader().pin().block(h(0)).is_none(), "empty store serves nothing");

        for height in h(0).up_to(h(7)) {
            store.append(height, hash(height), &framed(height)).expect("append");
        }
        assert_eq!(store.appended(), Some(h(7)), "writer advanced");
        assert_eq!(store.finalized_tip(), None, "nothing visible before commit");
        assert!(store.reader().pin().block(h(3)).is_none(), "uncommitted record is not served");

        store.commit(sizes(h(7))).expect("commit");
        let tip_7 = BlockRef { hash: BlockHash::from(hash(h(7))), height: h(7) };
        assert_eq!(store.finalized_tip(), Some(tip_7));
        assert_eq!(store.sizes(), sizes(h(7)));

        let reader = store.reader();
        assert_eq!(reader.pin().block(h(0)).as_deref(), Some(&framed(h(0))[..]), "genesis");
        assert_eq!(reader.pin().block(h(3)).as_deref(), Some(&framed(h(3))[..]), "record 3");
        let snapshot = reader.snapshot.load();
        let (window, reach) = snapshot.span_from(h(2), h(5), usize::MAX).expect("span");
        let expected: Vec<u8> = h(2).up_to(h(5)).flat_map(framed).collect();
        assert_eq!((window.as_ref(), reach), (&expected[..], h(5)));
        let (window, reach) = snapshot.span_from(h(2), h(5), 1).expect("budget-bound span");
        let one = (&framed(h(2))[..], h(2));
        assert_eq!((window.as_ref(), reach), one, "always one record");
        assert!(snapshot.span_from(h(8), h(9), usize::MAX).is_none(), "past the tail");
        drop(snapshot);

        drop(store);
        let mut reopened = CompactBlockStore::open(fs, path, NET).expect("reopen");
        assert_eq!(reopened.finalized_tip(), Some(tip_7));
        assert_eq!(reopened.sizes(), sizes(h(7)));
        assert_eq!(reopened.reader().pin().block(h(3)).as_deref(), Some(&framed(h(3))[..]));
        reopened.commit(sizes(h(7))).expect("nothing appended: no-op");
        reopened.append(h(8), hash(h(8)), &framed(h(8))).expect("append after reopen");
        reopened.commit(sizes(h(8))).expect("commit");
        assert_eq!(reopened.finalized_height(), Some(h(8)));
        assert_eq!(reopened.reader().pin().block(h(8)).as_deref(), Some(&framed(h(8))[..]));
    }

    /// Three commits crashed after every operation: each state reopens to an acknowledged or
    /// attempted commit, byte-identical to the model, and accepts the next commit
    #[test]
    fn every_crash_state_reopens_to_a_committed_prefix_that_keeps_appending() {
        let fs = SimFs::recording();
        let path = Path::new("/cb");
        let commits = [(0, 2), (3, 4), (5, 5)];
        {
            let mut store = CompactBlockStore::open(fs.clone(), path, NET).expect("open");
            for (acked, (first, last)) in (1u64..).zip(commits) {
                for height in h(first).up_to(h(last)) {
                    store.append(height, hash(height), &framed(height)).expect("append");
                }
                store.commit(sizes(h(last))).expect("commit");
                fs.set_tag(acked);
            }
        }
        // last committed height, inclusive, after `commits_done` commits (`None` = empty)
        let tip_after = |commits_done: u64| match commits_done {
            0 => None,
            1 => Some(h(2)),
            2 => Some(h(4)),
            _ => Some(h(5)),
        };

        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let mut store = CompactBlockStore::open(state.fs, path, NET)
                .unwrap_or_else(|error| panic!("{label}: {error}"));
            let tip = store.finalized_height();
            let acked = [tip_after(state.tag), tip_after(state.tag + 1)];
            assert!(acked.contains(&tip), "{label}: recovered tip {tip:?}");
            let pinned = store.reader().pin();
            for height in tip.into_iter().flat_map(|last| h(0).up_to(last)) {
                let served = pinned.block(height).map(Vec::from);
                assert_eq!(served, Some(framed(height)), "{label}: {height}");
            }
            let expected =
                tip.map(|height| BlockRef { hash: BlockHash::from(hash(height)), height });
            assert_eq!(store.finalized_tip(), expected, "{label}");

            let next = tip.map_or(Height::GENESIS, Height::next);
            store.append(next, hash(next), &framed(next)).expect("append");
            store.commit(sizes(next)).expect("commit after recovery");
            let served = store.reader().pin().block(next).map(Vec::from);
            assert_eq!(served, Some(framed(next)), "{label}: appends continue at the end");
        }
    }

    /// Open checks lengths and tail pages only: a lost file or a torn tail page is refused, bytes
    /// past the manifest are dropped, data without a manifest is refused
    #[test]
    fn open_checks_lengths_and_tail_pages_only() {
        let path = Path::new("/cb");
        let populated = || {
            let fs = SimFs::new();
            let mut store = CompactBlockStore::open(fs.clone(), path, NET).expect("open");
            for height in h(0).up_to(h(3)) {
                store.append(height, hash(height), &framed(height)).expect("append");
            }
            store.commit(sizes(h(3))).expect("commit");
            fs
        };
        let refused = |edit: &dyn Fn(&SimFs)| {
            let fs = populated();
            edit(&fs);
            CompactBlockStore::open(fs, path, NET).expect_err("refused").to_string()
        };
        let data_len: u64 = h(0).up_to(h(3)).map(|height| framed(height).len() as u64).sum();

        let short = refused(&|fs| fs.corrupt(&path.join(BLOCKS), |bytes| bytes.truncate(10)));
        let torn = refused(&|fs| fs.corrupt(&path.join(OFFSETS), |bytes| bytes[3] ^= 1));
        let manifest = refused(&|fs| fs.corrupt(&path.join("MANIFEST"), |bytes| bytes[20] ^= 1));
        let lost = format!("/cb/blocks.dat is 10 bytes, the committed state needs {data_len}");
        assert_eq!(short, lost);
        assert_eq!(torn, "/cb/offsets.idx: tail page fails its checksum");
        assert_eq!(manifest, "MANIFEST checksum mismatch");

        // bytes past the manifest = uncommitted: dropped, never an error
        let fs = populated();
        fs.corrupt(&path.join(BLOCKS), |bytes| bytes.extend_from_slice(&[0xa5; 64]));
        let store = CompactBlockStore::open(fs.clone(), path, NET).expect("open");
        assert_eq!(store.finalized_height(), Some(h(3)));
        let on_disk = fs.contents(&path.join(BLOCKS)).expect("blocks").len() as u64;
        assert_eq!(on_disk, data_len, "uncommitted tail truncated");

        drop(store);

        // data but no manifest = never committed by this code
        let bare = SimFs::new();
        bare.create_dir_all(path).expect("dir");
        bare.open(&path.join(BLOCKS)).expect("blocks").write_all_at(&[1], 0).expect("write");
        let opened = CompactBlockStore::open(bare, path, NET);
        assert!(matches!(opened, Err(StoreError::Manifest(ManifestError::Unmanifested { .. }))));
    }
}
