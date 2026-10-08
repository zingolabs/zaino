//! CrossIndex × Append: the entry at height `h` is twice the `value` toy's
//! entry at `h`.
//!
//! The in-crate archetype the [`CrossBridge`](crate::bridge) exists for. It
//! declares [`ValueIndex`] as its only
//! dependency and, for each block, point-reads that block's `value` entry
//! through the [`DepsReader`] and stores double it. Composition is
//! [`Append`]: one disjoint `height → 2·value` entry
//! per block.
//!
//! It exercises the cross-index row end to end: the scheduler releases its batch
//! only after `value` has persisted that batch (the `Pipelined` gate), and its
//! extraction reads `value`'s batch output through the `DepsReader` overlay
//! before the shared atomic commit.

use super::value_index::{self, ValueIndex};
use crate::descriptor::{Append, CrossIndex};
use crate::primitives::{BlockHeight, IndexId};
use crate::traits::{DepsReadError, DepsReader, ExtractCross, IndexDef, MergeAppend, Schema};
use zaino_persistence_codec::keys::HeightKey;
use zaino_persistence_codec::{
    DecodeError as PersistDecodeError, EntryCodec, KeyOrder, PersistentRecord,
};

/// Block context: just the block's height — the key into the `value` dependency.
pub struct Context {
    /// Block height.
    pub height: BlockHeight,
}

/// One height → doubled-value entry.
pub struct Entry {
    /// Block height (key).
    pub height: BlockHeight,
    /// Twice the `value` toy's entry at this height.
    pub doubled: u32,
}

/// Stores (height → 2 × value) by reading the `value` index's output.
pub struct CrossDoubleIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("cross_double");

impl IndexDef for CrossDoubleIndex {
    type Scope = CrossIndex;
    type Composition = Append;
    type Delta = Entry;
    type BlockContext = Context;

    const NAME: IndexId = ID;
    const DEPENDENCIES: &'static [IndexId] = &[value_index::ID];
}

/// Why the cross toy's extraction can fail.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading the `value` dependency failed.
    #[error("reading the value dependency")]
    Deps(#[source] DepsReadError),
    /// The `value` dependency had no entry at this height — it should always,
    /// since the gate opens only after `value` has persisted the batch.
    #[error("value dependency has no entry at height {height}")]
    MissingDependency {
        /// The height with no dependency entry.
        height: BlockHeight,
    },
}

impl From<DepsReadError> for Error {
    fn from(err: DepsReadError) -> Self {
        Self::Deps(err)
    }
}

impl ExtractCross for CrossDoubleIndex {
    type Error = Error;

    fn extract(ctx: &Context, deps: &DepsReader<'_>) -> Result<Entry, Self::Error> {
        let value = deps
            .get::<ValueIndex>(&ctx.height)?
            .ok_or(Error::MissingDependency { height: ctx.height })?;
        Ok(Entry {
            height: ctx.height,
            doubled: value.value() * 2,
        })
    }
}

impl MergeAppend for CrossDoubleIndex {}

impl Schema<Vec<Entry>> for CrossDoubleIndex {
    fn into_entries(entries: Vec<Entry>) -> Vec<(Self::Key, Self::Value)> {
        entries
            .into_iter()
            .map(|entry| (entry.height, entry.doubled))
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<Entry> {
        entries
            .into_iter()
            .map(|(height, doubled)| Entry { height, doubled })
            .collect()
    }
}

impl EntryCodec for CrossDoubleIndex {
    type Key = BlockHeight;
    type Value = u32;
    // The key is a plain block height — reuse the shared height record.
    type PersistentKey = HeightKey<BlockHeight>;
    type PersistentValue = PersistentDoubled;

    const KEY_ORDER: KeyOrder = KeyOrder::WalkOrdered;

    fn fingerprint_samples() -> Vec<(BlockHeight, u32)> {
        vec![(BlockHeight::new(1), 4)]
    }
}

/// On-disk record for the doubled value: a single `u32` little-endian.
#[derive(PersistentRecord)]
pub struct PersistentDoubled(u32);

impl PersistentRecord for PersistentDoubled {
    type Domain = u32;

    fn from_domain(domain: &u32) -> Self {
        Self(*domain)
    }
    fn into_domain(self) -> Result<u32, PersistDecodeError> {
        Ok(self.0)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use super::super::value_index::ValueIndex;
    use super::{CrossDoubleIndex, ID};
    use crate::backend::{Backend, BackendWriter, CommitError, FlushError, OpenError, WriteOp};
    use crate::engine::{EngineConfig, SyncEngine};
    use crate::index_pipelines::IndexPipelines;
    use crate::primitives::BlockHeight;
    use crate::testing::{InMemoryBackend, TestBlockContext};
    use crate::traits::{DepsReadError, DepsReader, IndexDef, PendingOverlay};

    /// Build a 0..`n` chain whose `value` toy stores `height + 1` at each height.
    fn chain(n: u64) -> Vec<TestBlockContext> {
        (0..n)
            .map(|height| TestBlockContext {
                height,
                value: u32::try_from(height).expect("toy height fits u32") + 1,
            })
            .collect()
    }

    /// (a) The cross index produces 2× the dependency's values for 40 blocks in
    /// batches of 6.
    #[test]
    fn cross_index_doubles_dependency_values_in_batches() {
        let backend = InMemoryBackend::new();
        let set = IndexPipelines::new()
            .with::<ValueIndex>()
            .with::<CrossDoubleIndex>();
        let mut engine = SyncEngine::from_pipelines(
            set,
            backend.clone(),
            EngineConfig {
                batch_size: 6,
                start_height: BlockHeight::new(0),
            },
        )
        .expect("valid index set");

        engine.sync_range(chain(40)).expect("sync succeeds");

        for height in 0..40u64 {
            let key =
                zaino_persistence_codec::encode_key::<CrossDoubleIndex>(&BlockHeight::new(height));
            let raw = backend
                .get_value(ID.into(), &key)
                .expect("cross entry present at every height");
            let got = zaino_persistence_codec::decode_value::<CrossDoubleIndex>(&raw)
                .expect("cross value decodes");
            let expected = (u32::try_from(height).expect("fits") + 1) * 2;
            assert_eq!(got, expected, "height {height}");
        }
    }

    /// (b) `DepsReader::get` on an index outside the declared dependency set
    /// returns `Undeclared`.
    #[test]
    fn deps_reader_refuses_an_undeclared_dependency() {
        let backend = InMemoryBackend::new();
        let reader = backend.reader().expect("reader");
        let overlay = PendingOverlay::default();
        // `value` is NOT in the allowed set, so reading it is refused.
        let allowed: [crate::primitives::IndexId; 0] = [];
        let deps = DepsReader::new(&reader, &overlay, &allowed);

        let result = deps.get::<ValueIndex>(&BlockHeight::new(0));
        assert!(
            matches!(
                result,
                Err(DepsReadError::Undeclared { dependency }) if dependency == ValueIndex::NAME
            ),
            "expected Undeclared for an index outside the dependency set",
        );
    }

    /// The `(namespace, key)` pairs of one atomic commit.
    type CommitRecord = Vec<(String, Vec<u8>)>;
    /// A shared log of every atomic commit's [`CommitRecord`], in commit order.
    type CommitLog = Arc<Mutex<Vec<CommitRecord>>>;

    /// Backend wrapper recording the `(namespace, key)` set of every atomic
    /// commit, so a test can assert what lands together.
    #[derive(Clone)]
    struct RecordingBackend {
        inner: InMemoryBackend,
        commits: CommitLog,
    }

    impl RecordingBackend {
        fn new() -> Self {
            Self {
                inner: InMemoryBackend::new(),
                commits: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Backend for RecordingBackend {
        type Reader = <InMemoryBackend as Backend>::Reader;
        type Writer = RecordingWriter;

        fn reader(&self) -> Result<Self::Reader, OpenError> {
            self.inner.reader()
        }

        fn writer(&self) -> Result<Self::Writer, OpenError> {
            Ok(RecordingWriter {
                inner: self.inner.writer()?,
                commits: Arc::clone(&self.commits),
            })
        }

        fn flush(&self) -> Result<(), FlushError> {
            self.inner.flush()
        }
    }

    struct RecordingWriter {
        inner: <InMemoryBackend as Backend>::Writer,
        commits: CommitLog,
    }

    impl BackendWriter for RecordingWriter {
        fn commit(&mut self, ops: Vec<WriteOp>) -> Result<(), CommitError> {
            let record: CommitRecord = ops
                .iter()
                .map(|op| match op {
                    WriteOp::Put { namespace, key, .. } => {
                        (namespace.as_str().to_owned(), key.clone())
                    }
                    WriteOp::Delete { namespace, key } => {
                        (namespace.as_str().to_owned(), key.clone())
                    }
                })
                .collect();
            self.commits
                .lock()
                .expect("commits mutex poisoned")
                .push(record);
            self.inner.commit(ops)
        }
    }

    /// (c) Per R1: an X extraction for batch β reads the values the dependency
    /// produced for β (the doubling proves the overlay read), and β commits
    /// atomically with both indexes' entries — every atomic commit carrying a
    /// cross entry for height `h` also carries `value`'s entry for `h`.
    #[test]
    fn cross_batch_commits_atomically_with_its_dependency() {
        let backend = RecordingBackend::new();
        let set = IndexPipelines::new()
            .with::<ValueIndex>()
            .with::<CrossDoubleIndex>();
        let mut engine = SyncEngine::from_pipelines(
            set,
            backend.clone(),
            EngineConfig {
                batch_size: 6,
                start_height: BlockHeight::new(0),
            },
        )
        .expect("valid index set");

        engine.sync_range(chain(12)).expect("sync succeeds");

        let value_ns = ValueIndex::NAME.as_str();
        let cross_ns = CrossDoubleIndex::NAME.as_str();
        let commits = backend
            .commits
            .lock()
            .expect("commits mutex poisoned")
            .clone();

        let mut cross_entries_seen = 0;
        for commit in &commits {
            let value_keys: HashSet<&Vec<u8>> = commit
                .iter()
                .filter(|(ns, _)| ns == value_ns)
                .map(|(_, key)| key)
                .collect();
            for (ns, key) in commit {
                if ns == cross_ns {
                    cross_entries_seen += 1;
                    assert!(
                        value_keys.contains(key),
                        "a cross entry committed without its value dependency in the \
                         same atomic batch",
                    );
                }
            }
        }
        assert_eq!(
            cross_entries_seen, 12,
            "every height produced a cross entry"
        );

        // The doubling proves the cross index read `value`'s batch output through
        // the overlay before the shared commit.
        for height in 0..12u64 {
            let key =
                zaino_persistence_codec::encode_key::<CrossDoubleIndex>(&BlockHeight::new(height));
            let raw = backend
                .inner
                .get_value(ID.into(), &key)
                .expect("cross entry present");
            let got = zaino_persistence_codec::decode_value::<CrossDoubleIndex>(&raw)
                .expect("cross value decodes");
            assert_eq!(got, (u32::try_from(height).expect("fits") + 1) * 2);
        }
    }
}
