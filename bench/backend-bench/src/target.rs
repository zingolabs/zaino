//! A backend a benchmark can stand up, populate and measure.
//!
//! [`BenchTarget`] is the whole of what a workload needs from an
//! implementation: create one fresh, hand out the [`Backend`], and say what it
//! occupies on disk. Every workload is generic over it, so adding the planned
//! positional/mmap backend to the comparison is one `impl` and one registration
//! line — no import to edit, no feature to flip, and no workload to touch.
//!
//! # What each target opens with
//!
//! - [`Lmdb`] opens a fresh environment in a `tempfile` directory that is
//!   removed when the target drops, with a map size of
//!   [`LMDB_MAP_SIZE_GB`] GiB. LMDB reserves the map up front, so the figure
//!   bounds how much a run may write, not how much it does write.
//! - [`InMemory`] opens the in-memory backend and reports zero on-disk bytes.
//!   It is the no-IO ceiling: the cost of the seam itself — key encoding, the
//!   batch vector, the map lookups — with the storage removed. Any real backend
//!   is slower, and how much slower is the number that matters.
//!
//! # Durability
//!
//! Every write workload ends with [`Backend::flush`], because the sync engine
//! does: it commits each batch and flushes once the run completes. A comparison
//! that skipped the flush would credit a backend for work it had not finished,
//! so the flush is inside the measured section.

use std::path::Path;

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_indexes::index_set::IndexSet;
use zaino_indexes::sets::compact_blocks::CompactBlocks;
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, Namespace};
use zaino_persistence_codec::reserved_namespaces;

use crate::error::BenchError;

/// The LMDB map size a benchmark reserves, in GiB. Large enough for the biggest
/// scale the env override allows, small enough to be a sparse file on any
/// filesystem the repo builds on.
pub const LMDB_MAP_SIZE_GB: usize = 8;

/// Every namespace a compact-block deployment declares: one per index in the
/// set the light-wallet deployment builds, plus the engine's reserved watermark
/// and format-version namespaces.
///
/// Taken from the real index set rather than listed here, so a set that gains an
/// index cannot leave the benchmark writing into a namespace the store would not
/// have opened.
pub fn namespaces() -> Vec<Namespace> {
    CompactBlocks::pipelines()
        .index_ids()
        .into_iter()
        .map(Namespace::from)
        .chain(reserved_namespaces())
        .collect()
}

/// One backend under measurement.
pub trait BenchTarget: Sized {
    /// The backend this target opens.
    type Backend: Backend;

    /// The name the benchmark reports this backend under.
    const NAME: &'static str;

    /// Open a fresh, empty instance. Called outside the measured section.
    fn create() -> Result<Self, BenchError>;

    /// The backend the workloads read and write through.
    fn backend(&self) -> &Self::Backend;

    /// Bytes this instance occupies on disk, or zero for a backend that holds
    /// nothing on disk. Read after a write workload, beside the logical bytes
    /// written, so write amplification is visible.
    fn on_disk_bytes(&self) -> u64;
}

/// The LMDB backend in a temporary directory.
pub struct Lmdb {
    // Dropped last, after the environment, so the directory outlives its use.
    directory: tempfile::TempDir,
    backend: LmdbBackend,
}

impl BenchTarget for Lmdb {
    type Backend = LmdbBackend;

    const NAME: &'static str = "lmdb";

    fn create() -> Result<Self, BenchError> {
        let directory = tempfile::tempdir().map_err(BenchError::Directory)?;
        let backend = LmdbBackend::open(LmdbConfig {
            path: directory.path().to_path_buf(),
            map_size_bytes: LMDB_MAP_SIZE_GB << 30,
            namespaces: namespaces(),
        })?;
        Ok(Self { directory, backend })
    }

    fn backend(&self) -> &Self::Backend {
        &self.backend
    }

    fn on_disk_bytes(&self) -> u64 {
        directory_bytes(self.directory.path())
    }
}

/// The in-memory backend: the no-IO ceiling.
pub struct InMemory(InMemoryBackend);

impl BenchTarget for InMemory {
    type Backend = InMemoryBackend;

    const NAME: &'static str = "in-memory";

    fn create() -> Result<Self, BenchError> {
        Ok(Self(InMemoryBackend::new()))
    }

    fn backend(&self) -> &Self::Backend {
        &self.0
    }

    fn on_disk_bytes(&self) -> u64 {
        0
    }
}

/// Bytes held by the files under `path`, walked one level deep — which is where
/// LMDB keeps its data and lock files, and where a positional backend would keep
/// its page files and manifest.
///
/// Apparent length, not allocated blocks: LMDB's map is sparse, so the allocated
/// size understates what a full environment would occupy while the apparent
/// length tracks what has actually been written into.
fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence::{BackendReader, BackendWriter, WriteOp};

    /// A namespace the benchmark writes but the backend never opened would fail
    /// at commit time on LMDB, so the set must cover every index plus the
    /// engine's reserved namespaces.
    #[test]
    fn the_declared_namespaces_cover_the_index_set_and_the_reserved_pair() {
        let declared = namespaces();

        for reserved in reserved_namespaces() {
            assert!(declared.contains(&reserved), "{reserved} not declared");
        }
        for id in CompactBlocks::pipelines().index_ids() {
            let namespace = Namespace::from(id);
            assert!(declared.contains(&namespace), "{namespace} not declared");
        }
    }

    /// Both targets must satisfy the same round trip, or a workload's numbers
    /// would not be comparable between them.
    #[test]
    fn every_target_round_trips_a_write_through_the_seam() -> Result<(), BenchError> {
        fn round_trip<T: BenchTarget>() -> Result<(), BenchError> {
            let target = T::create()?;
            let namespace = namespaces()[0];
            let mut writer = target.backend().writer()?;
            writer.commit(vec![WriteOp::Put {
                namespace,
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }])?;
            target.backend().flush()?;

            let read = target.backend().reader()?.get(namespace, b"k")?;
            assert_eq!(read.as_deref(), Some(b"v".as_slice()), "{}", T::NAME);
            Ok(())
        }

        round_trip::<InMemory>()?;
        round_trip::<Lmdb>()
    }

    #[test]
    fn lmdb_reports_on_disk_bytes_and_in_memory_reports_none() -> Result<(), BenchError> {
        assert_eq!(InMemory::create()?.on_disk_bytes(), 0);
        // A freshly opened environment has already written its data and lock
        // files, so the figure is non-zero before any batch lands.
        assert!(Lmdb::create()?.on_disk_bytes() > 0);
        Ok(())
    }
}
