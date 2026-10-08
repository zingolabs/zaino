//! Correcting a watermark that claims more than the index holds.
//!
//! The watermark is a stamp beside the data: the highest height every index
//! in the set has committed. The data outranks it. A watermark *below* the
//! data is the ordinary crash-recovery state and the indexer re-covers the
//! difference. A watermark *above* the data is corruption of the stamp alone,
//! and it is not harmless: the composed view routes every height up to the
//! watermark to this store, so each height in the gap is answered "no such
//! block" while the chain head holds it, and the indexer resumes from a
//! height it never reached, leaving the gap unindexed for good.
//!
//! The repair is the only sane one: find the highest header the index
//! actually holds, check the run below it is unbroken, and re-stamp the
//! watermark at the top of the unbroken run. A hole below the top is the
//! other shape the same fault leaves — a range that failed to index while a
//! later one succeeded — and a watermark above a hole makes the indexer skip
//! it forever. Heights beyond the window are trusted: they were written by
//! the bulk catch-up, whose batches are contiguous by construction.
//!
//! Lowering the stamp also lowers the data it stands for. The indexer resumes
//! from just past the corrected stamp and re-appends those heights; a
//! walk-ordered index that still held them would reject the re-append as out of
//! order (its keys are written with a sorted append). So the repair first
//! trims every walk-ordered namespace down to the corrected height — the stale
//! heights above it — and only then stamps, with the trims made durable first
//! so a crash leaves the old, higher stamp and the repair re-runs. Scattered
//! namespaces are left alone: replay re-puts their keys with a plain last-wins
//! put.
//!
//! It is checked on every boot, before anything reads the stamp, and
//! reported loudly when it fires.
//!
//! ```text
//! held(h)    ⟺  headers[h] present
//! top        =   max { h ≤ w : held(h) }
//! repaired   =   min { g < top : ¬held(g) ∧ top − g ≤ WINDOW } − 1    if such g
//!            |   top                                                    otherwise
//! ```

use core::ops::ControlFlow;

use zaino_indexes::index_set::Builds;
use zaino_indexes::indexes::headers::{self, HeadersIndex};
use zaino_persistence::{
    Backend, BackendReader, BackendWriter, CommitError, KeyOrder, Namespace, OpenError, RawKey,
    WriteOp,
};
use zaino_persistence_codec::watermark;
use zaino_primitives::types::Height;

use crate::{read_index_value, StoreReader};

/// How far below a bad watermark the repair looks for a header before giving
/// up. A gap this size is not a stamp fault but an index that never held the
/// heights it claims, which a re-stamp would only paper over.
const MAX_SEARCH: u32 = 1 << 20;

/// How far below the top header the repair checks for holes. Wider than any
/// run a follow sync writes between two stamps, so a range that failed to
/// index behind a later one that succeeded is inside it.
const WINDOW: u32 = 1 << 14;

/// A watermark the headers index did not bear out, and where it now points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatermarkRepair {
    /// The height the stamp claimed.
    pub claimed: Height,
    /// The top of the unbroken run of headers, which the stamp now names.
    pub corrected: Height,
    /// How many stale entries were trimmed from each walk-ordered namespace to
    /// match the lowered watermark, in index declaration order. Empty when the
    /// correction only moved the stamp and no walk-ordered namespace held
    /// heights above `corrected`.
    pub trimmed: Vec<(Namespace, usize)>,
}

/// The watermark could not be checked against the index, or not corrected.
#[derive(Debug, thiserror::Error)]
pub enum WatermarkRepairError {
    /// The backend could not open a reader or a writer.
    #[error("opening the store")]
    Open(#[source] OpenError),

    /// The watermark itself could not be read.
    #[error("reading the watermark")]
    Read(#[source] zaino_persistence::ReadError),

    /// A header probe failed for a reason other than absence.
    #[error("probing the headers index: {0}")]
    Probe(String),

    /// The corrected stamp could not be written.
    #[error("re-stamping the watermark")]
    Commit(#[source] CommitError),

    /// Scanning a walk-ordered namespace for entries above the corrected
    /// watermark failed.
    #[error("scanning {namespace} for entries to trim")]
    TrimScan {
        /// The walk-ordered namespace being scanned.
        namespace: String,
        /// The read failure.
        #[source]
        source: zaino_persistence::ReadError,
    },

    /// Deleting the entries above the corrected watermark failed.
    #[error("trimming {namespace} to the corrected watermark")]
    TrimCommit {
        /// The walk-ordered namespace being trimmed.
        namespace: String,
        /// The delete-batch failure.
        #[source]
        source: CommitError,
    },

    /// The trimmed entries could not be made durable before stamping the
    /// corrected watermark. The stamp must not outlive untrimmed data on disk,
    /// so the repair aborts rather than lower the watermark over heights that
    /// resume would re-append.
    #[error("flushing trimmed entries before stamping the corrected watermark")]
    Flush(#[source] zaino_persistence::FlushError),

    /// No header exists at the watermark or within `MAX_SEARCH` heights
    /// below it: the index does not hold what the stamp claims, and by more
    /// than a stamp fault could explain.
    #[error(
        "watermark {claimed} is ahead of the headers index and no header was found within \
         {searched} heights below it"
    )]
    NoHeaderBelow {
        /// The height the stamp claimed.
        claimed: Height,
        /// How many heights below it were probed.
        searched: u32,
    },
}

impl<B, M> StoreReader<B, M>
where
    B: Backend + 'static,
    M: Builds<HeadersIndex>,
{
    /// Check the watermark against the headers index and re-stamp it at the
    /// top of the unbroken run of headers if it claims more.
    ///
    /// `Ok(None)` when the stamp is consistent with the data (or there is no
    /// stamp); `Ok(Some(repair))` when it was corrected. The write is a
    /// single committed operation, so a crash mid-repair leaves either stamp,
    /// never neither.
    pub fn repair_watermark(&self) -> Result<Option<WatermarkRepair>, WatermarkRepairError> {
        let reader = self.backend.reader().map_err(WatermarkRepairError::Open)?;
        let Some(claimed) = watermark::read(&reader).map_err(WatermarkRepairError::Read)? else {
            return Ok(None);
        };

        // The highest header at or below the claim.
        let mut top = claimed;
        let mut searched = 0u32;
        while !header_held::<B>(&reader, top)? {
            searched += 1;
            let below = top.checked_sub(1).filter(|_| searched <= MAX_SEARCH);
            let Some(below) = below else {
                return Err(WatermarkRepairError::NoHeaderBelow { claimed, searched });
            };
            top = below;
        }

        // The lowest hole within the window below it, if any: the run the
        // stamp may name ends just beneath it.
        let mut corrected = top;
        let mut probe = top;
        for _ in 0..WINDOW {
            let Some(below) = probe.checked_sub(1) else {
                break;
            };
            if !header_held::<B>(&reader, below)? {
                corrected = below
                    .checked_sub(1)
                    .ok_or(WatermarkRepairError::NoHeaderBelow { claimed, searched })?;
            }
            probe = below;
        }

        if corrected == claimed {
            return Ok(None);
        }

        // Lowering the stamp without lowering the data is not enough: the
        // indexer resumes from `corrected + 1` and re-appends those heights,
        // but the walk-ordered indexes still hold them, and an append of a key
        // that already exists is rejected (`OutOfOrderAppend`) rather than
        // overwriting as a plain put once did. Trim the stale heights first.
        let trimmed = self.trim_walk_ordered_above(corrected)?;
        if !trimmed.is_empty() {
            // Make the deletes durable before the stamp moves. If a crash lands
            // between the two, the old (higher) watermark must survive so the
            // repair re-runs; otherwise a backend that may reorder the stamp
            // ahead of the deletes (LMDB under `NO_SYNC`) could leave a lowered
            // stamp over data resume would then collide with.
            self.backend.flush().map_err(WatermarkRepairError::Flush)?;
        }

        let mut writer = self.backend.writer().map_err(WatermarkRepairError::Open)?;
        writer
            .commit(vec![watermark::stamp(corrected)])
            .map_err(WatermarkRepairError::Commit)?;
        Ok(Some(WatermarkRepair {
            claimed,
            corrected,
            trimmed,
        }))
    }

    /// Delete every walk-ordered entry strictly above `corrected`, so the index
    /// contents match the lowered watermark before resume appends from
    /// `corrected + 1`. Returns the per-namespace count removed (zero-count
    /// namespaces omitted).
    ///
    /// Walk-ordered namespaces are height-keyed big-endian (the
    /// [`KeyOrder::WalkOrdered`] contract), so the keys to drop are exactly the
    /// half-open byte range `[encode(corrected + 1), ∞)`. Scattered namespaces
    /// are left untouched: replay re-puts their keys with a plain last-wins put,
    /// which needs no trim. The namespaces and their orders come from the index
    /// set's own specs, so no index list is hard-coded here.
    fn trim_walk_ordered_above(
        &self,
        corrected: Height,
    ) -> Result<Vec<(Namespace, usize)>, WatermarkRepairError> {
        // The first stale height sits just above the corrected tip. `u64`
        // arithmetic cannot overflow a `u32` height, and the 8-byte big-endian
        // bound matches the `HeightKey` layout every walk-ordered index writes.
        let first_stale = u64::from(u32::from(corrected)) + 1;
        let lower = first_stale.to_be_bytes().to_vec();
        // One past the largest possible 8-byte key; no real height reaches it,
        // so the half-open range drops every stale height and nothing else.
        let upper = [0xffu8; 8];

        let mut trimmed = Vec::new();
        for spec in M::pipelines().namespace_specs() {
            if spec.key_order != KeyOrder::WalkOrdered {
                continue;
            }
            let removed = self.trim_namespace_above(spec.namespace, &lower, &upper)?;
            if removed > 0 {
                trimmed.push((spec.namespace, removed));
            }
        }
        Ok(trimmed)
    }

    /// Delete the entries of `namespace` in the half-open byte range
    /// `[lower, upper)` in bounded chunks, returning how many were removed.
    ///
    /// Chunking bounds both the keys held in memory and the size of one delete
    /// batch, so a namespace holding many stale heights does not force a single
    /// unbounded commit. Each chunk deletes the keys it read, so the next scan
    /// from the same `lower` returns the following ones; the loop ends when a
    /// scan finds nothing.
    fn trim_namespace_above(
        &self,
        namespace: Namespace,
        lower: &[u8],
        upper: &[u8],
    ) -> Result<usize, WatermarkRepairError> {
        /// Entries deleted per trim commit.
        const TRIM_CHUNK: usize = 10_000;

        let mut removed = 0usize;
        loop {
            let reader = self.backend.reader().map_err(WatermarkRepairError::Open)?;
            let mut keys: Vec<RawKey> = Vec::new();
            reader
                .scan_range(namespace, lower, upper, &mut |key, _| {
                    keys.push(key.to_vec());
                    if keys.len() >= TRIM_CHUNK {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
                .map_err(|source| WatermarkRepairError::TrimScan {
                    namespace: namespace.to_string(),
                    source,
                })?;
            if keys.is_empty() {
                return Ok(removed);
            }
            let batch = keys.len();
            let ops = keys
                .into_iter()
                .map(|key| WriteOp::Delete { namespace, key })
                .collect();
            let mut writer = self.backend.writer().map_err(WatermarkRepairError::Open)?;
            writer
                .commit(ops)
                .map_err(|source| WatermarkRepairError::TrimCommit {
                    namespace: namespace.to_string(),
                    source,
                })?;
            removed += batch;
        }
    }
}

/// Whether the headers index holds a header at `height`. A stale index reads
/// as absent, so a stale store is not silently re-stamped as healthy.
fn header_held<B: Backend>(
    reader: &B::Reader,
    height: Height,
) -> Result<bool, WatermarkRepairError> {
    read_index_value::<HeadersIndex, B>(reader, headers::ID.into(), height)
        .map(|header| header.is_some())
        .map_err(|transient| WatermarkRepairError::Probe(transient.0))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
    use zaino_indexes::index_set::IndexSet;
    use zaino_indexes::indexes::headers::{self, HeaderValue, HeadersIndex};
    use zaino_indexes::indexes::txids::{self, TxidsIndex};
    use zaino_indexes::sets::compact_blocks::CompactBlocks;
    use zaino_persistence::{Backend, BackendReader, BackendWriter, NamespaceSpec, WriteOp};
    use zaino_persistence_codec::{
        encode_key, encode_value, reserved_namespaces, version_stamp, watermark,
    };
    use zaino_primitives::types::{BlockHash, CompactDifficulty, Height};
    use zaino_sync::primitives::BlockHeight;

    use super::StoreReader;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    /// Open an LMDB store with every namespace `M` writes plus the reserved
    /// meta namespaces — the set the runtime's `open_store` declares.
    fn open_at<M: IndexSet>(path: &Path) -> LmdbBackend {
        let namespaces: Vec<NamespaceSpec> = M::pipelines()
            .namespace_specs()
            .into_iter()
            .chain(reserved_namespaces().map(NamespaceSpec::meta))
            .collect();
        LmdbBackend::open(LmdbConfig {
            path: path.to_path_buf(),
            map_size_bytes: 16 << 20,
            namespaces,
        })
        .expect("open lmdb store")
    }

    /// A valid, decodable headers entry at `h`. The hash is constant: the repair
    /// only probes presence, not contents.
    fn header_put(h: u32) -> WriteOp {
        let value = HeaderValue {
            hash: BlockHash::from([1u8; 32]),
            prev_hash: BlockHash::from([2u8; 32]),
            time: 3,
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
        };
        WriteOp::Put {
            namespace: headers::ID.into(),
            key: encode_key::<HeadersIndex>(&BlockHeight::new(u64::from(h))),
            value: encode_value::<HeadersIndex>(&value),
        }
    }

    /// A txids entry at `h`, keyed as the index would key it. The value is
    /// opaque to the repair (it trims by key), so arbitrary bytes suffice.
    fn txid_put(h: u32) -> WriteOp {
        WriteOp::Put {
            namespace: txids::ID.into(),
            key: encode_key::<TxidsIndex>(&BlockHeight::new(u64::from(h))),
            value: vec![0xab],
        }
    }

    fn commit(backend: &LmdbBackend, ops: Vec<WriteOp>) {
        let mut writer = backend.writer().expect("writer");
        writer.commit(ops).expect("commit");
    }

    fn txid_present(backend: &LmdbBackend, h: u32) -> bool {
        let reader = backend.reader().expect("reader");
        reader
            .get(
                txids::ID.into(),
                &encode_key::<TxidsIndex>(&BlockHeight::new(u64::from(h))),
            )
            .expect("get")
            .is_some()
    }

    /// A watermark above the headers max, with another walk-ordered namespace
    /// (txids) still holding heights above the corrected tip: repair trims them
    /// and the next batch re-appends from `corrected + 1` without an
    /// out-of-order append failure (the regression this fix exists for).
    #[test]
    fn repair_trims_stale_walk_ordered_heights_so_resume_can_reappend() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = open_at::<CompactBlocks>(tmp.path());

        // Headers hold an unbroken run [0, 5]; txids hold [0, 8] (its 6..=8
        // survived a corruption that lost the matching headers). The stamp
        // claims 8. Headers version-stamped so the probe reads them as held.
        let mut ops = vec![version_stamp::<HeadersIndex>(headers::ID.into())];
        ops.extend((0..=5).map(header_put));
        ops.extend((0..=8).map(txid_put));
        ops.push(watermark::stamp(height(8)));
        commit(&backend, ops);
        backend.flush().expect("flush");

        assert!(txid_present(&backend, 6) && txid_present(&backend, 8));

        let reader = StoreReader::<_, CompactBlocks>::new(Arc::new(backend.clone()));
        let repair = reader
            .repair_watermark()
            .expect("repair runs")
            .expect("the watermark was ahead of the headers, so it is corrected");

        assert_eq!(repair.claimed, height(8));
        assert_eq!(
            repair.corrected,
            height(5),
            "corrected to the highest header held"
        );
        assert_eq!(
            repair.trimmed,
            vec![(txids::ID.into(), 3)],
            "exactly txids' three stale heights (6,7,8) were trimmed"
        );

        // The stale heights are gone; the surviving run is intact.
        assert!(
            !txid_present(&backend, 6) && !txid_present(&backend, 7) && !txid_present(&backend, 8)
        );
        assert!(txid_present(&backend, 5) && txid_present(&backend, 0));
        let reader_view = backend.reader().expect("reader");
        assert_eq!(
            watermark::read(&reader_view).expect("read watermark"),
            Some(height(5)),
            "the stamp now names the corrected tip"
        );

        // The regression guard: resume appends height 6 into txids. Before the
        // trim this collided with the surviving 6 (OutOfOrderAppend); now it
        // lands cleanly.
        commit(&backend, vec![txid_put(6)]);
        assert!(txid_present(&backend, 6));

        // Re-running the repair now that the stamp is consistent is a no-op.
        let reader = StoreReader::<_, CompactBlocks>::new(Arc::new(backend));
        assert!(
            reader.repair_watermark().expect("repair runs").is_none(),
            "a watermark consistent with the data is not corrected again"
        );
    }

    /// A watermark that matches the headers max is left alone: nothing is
    /// corrected, nothing is trimmed.
    #[test]
    fn repair_is_a_no_op_when_the_watermark_is_consistent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = open_at::<CompactBlocks>(tmp.path());

        let mut ops = vec![version_stamp::<HeadersIndex>(headers::ID.into())];
        ops.extend((0..=5).map(header_put));
        ops.extend((0..=5).map(txid_put));
        ops.push(watermark::stamp(height(5)));
        commit(&backend, ops);
        backend.flush().expect("flush");

        let reader = StoreReader::<_, CompactBlocks>::new(Arc::new(backend.clone()));
        assert!(
            reader.repair_watermark().expect("repair runs").is_none(),
            "a consistent watermark is not touched"
        );
        // Nothing was trimmed.
        assert!(txid_present(&backend, 5) && txid_present(&backend, 0));
    }
}
