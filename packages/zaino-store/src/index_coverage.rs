//! Refusing to open a store that would serve an index built from less than the
//! whole chain.
//!
//! Serviceability is derived from index *presence* (a format stamp recorded
//! beside the data), not from how much of the chain each index covers: the
//! store has one shared watermark, and every index is assumed to reach it.
//! That assumption holds for a store built from genesis in one pass, where
//! every index was stamped at the first block and has grown with the rest.
//!
//! It breaks when a deployment gains an index and opens a store that already
//! holds data. Sync resumes from the shared watermark; the new index is stamped
//! on its *first forward write* and then reports serviceable — while it holds
//! only `[resume, tip]`, never `[genesis, resume)`. A read it backs (address
//! history, say) is then silently incomplete, with no error.
//!
//! This guard makes that state fail loud instead. It is narrow by design: it
//! does **not** track per-index coverage. It asserts the one invariant that
//! keeps the shared-watermark assumption true — *an index can only be added to
//! a store that has synced nothing* — by refusing to open when any index the
//! deployment declares has no format stamp while the store has already
//! committed a watermark.
//!
//! An index stamped at an *older* codec version is a different matter: its
//! stamp is present, so this guard passes it, and the per-read freshness check
//! (`read_keyed`) already rejects a format skew. This guard is only about an
//! absent stamp on a store that has synced something.
//!
//! ```text
//! open(M, store) = refuse   if watermark(store) = Some(_) ∧ ∃ I ∈ declared(M) : ¬stamped(I)
//!                | allow     otherwise
//! ```

use zaino_indexes::index_set::IndexSet;
use zaino_persistence::{Backend, Namespace, OpenError, ReadError};
use zaino_persistence_codec::{recorded_version, watermark};
use zaino_primitives::types::{Height, IndexId};

use crate::StoreReader;

/// The declared indexes a store holds no format stamp for, in the index set's
/// declaration order. Renders as a comma-separated list in the boot error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnstampedIndexes(Vec<IndexId>);

impl std::fmt::Display for UnstampedIndexes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for id in &self.0 {
            if !first {
                f.write_str(", ")?;
            }
            f.write_str(id.as_str())?;
            first = false;
        }
        Ok(())
    }
}

/// Opening the store for this deployment's index set would serve incomplete data.
#[derive(Debug, thiserror::Error)]
pub enum IndexCoverageError {
    /// The backend reader could not be opened to check the stamps.
    #[error("opening the store to check index coverage")]
    Open(#[source] OpenError),

    /// The watermark itself could not be read.
    #[error("reading the store watermark")]
    Read(#[source] ReadError),

    /// A format stamp could not be read for one declared index.
    #[error("reading the format stamp for index `{index}`")]
    Probe {
        /// The index whose stamp could not be read.
        index: IndexId,
        /// Why the read failed.
        #[source]
        source: ReadError,
    },

    /// One or more indexes this deployment declares were never built into the
    /// store, which has already committed a watermark. Opening it would serve
    /// those indexes' reads as complete while they cover only part of the chain.
    #[error(
        "the store has already synced (watermark height {watermark}), but these indexes this \
         deployment declares were never built into it: {unstamped}. On a store that has synced \
         anything, a newly added index is stamped on its first forward write and would hold only \
         [resume, tip], not [genesis, tip] — so the reads it backs (for example address history) \
         would report serviceable while silently missing all history below the resume height. \
         Remedy: resync from genesis into a fresh, empty data directory."
    )]
    Incomplete {
        /// The watermark the store has committed to.
        watermark: Height,
        /// The declared indexes with no format stamp.
        unstamped: UnstampedIndexes,
    },
}

impl<B, M> StoreReader<B, M>
where
    B: Backend + 'static,
    M: IndexSet,
{
    /// Refuse to open the store when a declared index would serve incomplete
    /// data — a fail-loud boot guard, checked before anything reads or writes
    /// the indexes.
    ///
    /// `Ok(())` when the store is safe to open: either it has synced nothing
    /// (no watermark), so every declared index builds from genesis with the
    /// rest, or every declared index already bears a format stamp. `Err` when
    /// the store has a watermark but a declared index has none — it was added
    /// to a store that already holds data, so it would cover `[resume, tip]`
    /// only.
    ///
    /// Reopening with a **subset** of the stamped set is always safe here: the
    /// guard checks only the indexes `M` declares, so a stamped namespace this
    /// index set does not read is simply left untouched.
    pub fn check_index_coverage(&self) -> Result<(), IndexCoverageError> {
        let reader = self.backend.reader().map_err(IndexCoverageError::Open)?;

        // No watermark → the store has synced nothing. A declared index is
        // built from genesis alongside the rest, so adding one is safe.
        let Some(watermark) = watermark::read(&reader).map_err(IndexCoverageError::Read)? else {
            return Ok(());
        };

        let mut unstamped = Vec::new();
        for index in M::INDEXES {
            let namespace = Namespace::from(*index);
            let recorded = recorded_version(&reader, namespace).map_err(|source| {
                IndexCoverageError::Probe {
                    index: *index,
                    source,
                }
            })?;
            if recorded.is_none() {
                unstamped.push(*index);
            }
        }

        if unstamped.is_empty() {
            Ok(())
        } else {
            Err(IndexCoverageError::Incomplete {
                watermark,
                unstamped: UnstampedIndexes(unstamped),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
    use zaino_indexes::index_set::IndexSet;
    use zaino_indexes::indexes::address_history::{self, AddressHistoryIndex};
    use zaino_indexes::indexes::chain_metadata::{self, ChainMetadataIndex};
    use zaino_indexes::indexes::hash_to_height::{self, HashToHeightIndex};
    use zaino_indexes::indexes::headers::{self, HeadersIndex};
    use zaino_indexes::indexes::ironwood::{self, IronwoodIndex};
    use zaino_indexes::indexes::orchard::{self, OrchardIndex};
    use zaino_indexes::indexes::sapling::{self, SaplingIndex};
    use zaino_indexes::indexes::transparent_data::{self, TransparentDataIndex};
    use zaino_indexes::indexes::transparent_spends::{self, TransparentSpendsIndex};
    use zaino_indexes::indexes::txid_location::{self, TxidLocationIndex};
    use zaino_indexes::indexes::txids::{self, TxidsIndex};
    use zaino_indexes::sets::compact_blocks::CompactBlocks;
    use zaino_indexes::sets::transparent_history::TransparentHistory;
    use zaino_persistence::{Backend, BackendWriter, NamespaceSpec, WriteOp};
    use zaino_persistence_codec::{reserved_namespaces, version_stamp, watermark};
    use zaino_primitives::types::Height;

    use super::{IndexCoverageError, StoreReader};

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    /// Open an LMDB store at `path` with every namespace the index set `M`
    /// writes, plus the reserved watermark / version namespaces — the same set
    /// the runtime's `open_store` declares.
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

    /// The version stamps for the compact-block index set (set A).
    fn compact_block_stamps() -> Vec<WriteOp> {
        vec![
            version_stamp::<HeadersIndex>(headers::ID.into()),
            version_stamp::<TxidsIndex>(txids::ID.into()),
            version_stamp::<HashToHeightIndex>(hash_to_height::ID.into()),
            version_stamp::<TransparentDataIndex>(transparent_data::ID.into()),
            version_stamp::<SaplingIndex>(sapling::ID.into()),
            version_stamp::<OrchardIndex>(orchard::ID.into()),
            version_stamp::<IronwoodIndex>(ironwood::ID.into()),
            version_stamp::<ChainMetadataIndex>(chain_metadata::ID.into()),
        ]
    }

    /// The three transparent-history-only stamps on top of the compact set.
    fn transparent_history_extra_stamps() -> Vec<WriteOp> {
        vec![
            version_stamp::<AddressHistoryIndex>(address_history::ID.into()),
            version_stamp::<TransparentSpendsIndex>(transparent_spends::ID.into()),
            version_stamp::<TxidLocationIndex>(txid_location::ID.into()),
        ]
    }

    fn commit(backend: &LmdbBackend, ops: Vec<WriteOp>) {
        let mut writer = backend.writer().expect("writer");
        writer.commit(ops).expect("commit");
    }

    /// A store synced with the compact-block set, reopened by a deployment that
    /// declares the superset transparent-history set, is refused — and the
    /// error names exactly the three indexes that were never built, plus the
    /// watermark.
    #[test]
    fn superset_over_a_synced_store_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // Phase A: a store synced with the compact-block set, watermark past
        // genesis.
        {
            let backend = open_at::<CompactBlocks>(tmp.path());
            let mut ops = compact_block_stamps();
            ops.push(watermark::stamp(height(100)));
            commit(&backend, ops);
        }

        // Phase B: reopen as the transparent-history deployment would.
        let backend = open_at::<TransparentHistory>(tmp.path());
        let reader = StoreReader::<_, TransparentHistory>::new(Arc::new(backend));

        let err = reader
            .check_index_coverage()
            .expect_err("a superset over a synced store must be refused");
        match err {
            IndexCoverageError::Incomplete {
                watermark,
                unstamped,
            } => {
                assert_eq!(watermark, height(100));
                let named: Vec<&str> = unstamped.0.iter().map(|id| id.as_str()).collect();
                assert_eq!(named.len(), 3, "exactly the three added indexes: {named:?}");
                assert!(named.contains(&address_history::ID.as_str()));
                assert!(named.contains(&transparent_spends::ID.as_str()));
                assert!(named.contains(&txid_location::ID.as_str()));
                // The remedy is named for the operator.
                assert!(err_to_string(&IndexCoverageError::Incomplete {
                    watermark,
                    unstamped,
                })
                .contains("fresh, empty data directory"));
            }
            other => panic!("expected Incomplete, got {other}"),
        }
    }

    fn err_to_string(err: &IndexCoverageError) -> String {
        err.to_string()
    }

    /// The same superset over an empty store (never synced, no watermark) opens
    /// fine: the new indexes build from genesis with the rest.
    #[test]
    fn superset_over_an_empty_store_opens() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = open_at::<TransparentHistory>(tmp.path());
        let reader = StoreReader::<_, TransparentHistory>::new(Arc::new(backend));
        reader
            .check_index_coverage()
            .expect("an empty store opens for any index set");
    }

    /// Reopening with the same set that synced the store opens fine.
    #[test]
    fn same_set_reopens_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let backend = open_at::<CompactBlocks>(tmp.path());
            let mut ops = compact_block_stamps();
            ops.push(watermark::stamp(height(100)));
            commit(&backend, ops);
        }
        let backend = open_at::<CompactBlocks>(tmp.path());
        let reader = StoreReader::<_, CompactBlocks>::new(Arc::new(backend));
        reader
            .check_index_coverage()
            .expect("the same set reopens cleanly");
    }

    /// Reopening with a subset of the stamped set opens fine: dropping an index
    /// is safe — the extra stamped namespace is simply left unread.
    #[test]
    fn subset_reopens_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let backend = open_at::<TransparentHistory>(tmp.path());
            let mut ops = compact_block_stamps();
            ops.extend(transparent_history_extra_stamps());
            ops.push(watermark::stamp(height(100)));
            commit(&backend, ops);
        }
        let backend = open_at::<CompactBlocks>(tmp.path());
        let reader = StoreReader::<_, CompactBlocks>::new(Arc::new(backend));
        reader
            .check_index_coverage()
            .expect("a subset of the stamped set reopens cleanly");
    }
}
