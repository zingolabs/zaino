//! Persistence port traits + table read views + verify report (usage.md "What an index may ask")
//!
//! - finalized data only, insert only (+ single removes on a `deletes()` map), buffered then one
//!   atomic commit, pinned reads, verifiable
//! - tables: `schema.rs`, one block's delta: `changes.rs`
//! - engine's `View` reads each kind it holds

use std::{num::NonZeroUsize, ops::Range, path::Path};

use bytes::Bytes;
use serde::Serialize;
use zaino_primitives::types::BlockRef;

use crate::{
    changes::BlockChanges,
    error::{CommitFailed, StoreError},
    schema::{MapId, MapTable, Schema, SequenceId, SequenceTable},
    write_buffer::AppliedView,
};

/// Storage backend: one store per index, verified offline
pub trait PersistenceEngine: Send + Sync + 'static {
    type Store: Store;

    /// Store at `path`: created, or resumed at its durable tip
    ///
    /// - commits by itself once `write_buffer` bytes buffered
    /// - another identity there (kind, format, network) = error, never a reformat
    /// - table this engine cannot hold = panic naming it (tables = constants: a bug)
    fn open(
        &self,
        path: &Path,
        schema: &Schema,
        write_buffer: NonZeroUsize,
    ) -> Result<Self::Store, StoreError>;

    /// Every durable byte against its integrity data (read-only: safe beside a running writer)
    fn verify(&self, path: &Path, schema: &Schema) -> Result<Verification, StoreError>;
}

/// Index's store = commit point of all its tables (one writer)
///
/// - finalized data: [`apply`](Self::apply) buffers (commits itself on a full buffer),
///   [`commit`](Self::commit) makes the rest durable now
pub trait Store: Send + 'static {
    type View: PinnedView;

    /// As opened
    fn schema(&self) -> &Schema;

    /// Empty delta for block `at`, one buffer per table of [`schema`](Self::schema)
    fn changes(&self, at: BlockRef) -> BlockChanges {
        self.schema().changes(at)
    }

    /// `changes` buffered: in [`applied`](Self::applied), not in [`durable`](Self::durable)
    ///
    /// - buffer at `write_buffer` bytes → durable (as [`commit`](Self::commit))
    /// - panics (nothing buffered): changes for another schema, a tip not above the last applied,
    ///   a map key the buffer already holds or `changes` inserts twice
    /// - panics: that commit failing ([`CommitFailed::crash`]), or a store an earlier failed
    ///   commit poisoned
    fn apply(&mut self, changes: BlockChanges);

    /// Every buffered change + the last applied tip, durable together (one fsync), then in
    /// [`durable`](Self::durable)
    ///
    /// - nothing buffered = `Ok`, nothing written
    /// - `Err` poisons the store: every later apply / commit panics (failed sync never retried)
    fn commit(&mut self) -> Result<(), CommitFailed>;

    /// Durable only (what serving pins: no crash takes back what a reader saw)
    fn durable(&self) -> Self::View;

    /// Durable + buffered (what a bulk fold reads its parent through)
    fn applied(&self) -> AppliedView<'_, Self::View>;
}

/// State at a tip: [`PinnedView`] (served) or [`AppliedView`] (a writer's fold, borrowed)
pub trait View: Clone + Send + Sync {
    /// Last block in this view (`None` = none yet)
    fn tip(&self) -> Option<BlockRef>;

    /// Store's, as opened
    fn schema(&self) -> &Schema;
}

/// Reads of sequence tables (engine side by position, index side [`sequence`](Self::sequence))
pub trait SequenceRead: View {
    /// Records held (= the next position)
    fn len(&self, table: SequenceId) -> u64;

    /// Record `at` (`None` = at or past `len`)
    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes>;

    /// Records in `range`, in order (`range` within `len`)
    fn records(&self, table: SequenceId, range: Range<u64>) -> Vec<Bytes>;

    /// `table`'s reads (panics: not in [`View::schema`])
    fn sequence(&self, table: SequenceTable) -> SequenceView<'_, Self>
    where
        Self: Sized,
    {
        self.schema().sequence_at(table);
        SequenceView { view: self, table: table.id }
    }
}

/// Reads of map tables (engine side by position, index side [`map`](Self::map))
pub trait MapRead: View {
    /// Value under `key`
    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes>;

    /// One answer per key, in `keys` order
    fn values(&self, table: MapId, keys: &[&[u8]]) -> Vec<Option<Bytes>>;

    /// `(key, value)` for `start <= key < end`, in key order (`None` = more than `limit`)
    fn range(
        &self,
        table: MapId,
        start: &[u8],
        end: &[u8],
        limit: usize,
    ) -> Option<Vec<(Bytes, Bytes)>>;

    /// `table`'s reads (panics: not in [`View::schema`])
    fn map(&self, table: MapTable) -> MapView<'_, Self>
    where
        Self: Sized,
    {
        self.schema().map_at(table);
        MapView { view: self, table: table.id }
    }
}

/// gRPC's read: pinned state, fixed while held, owned (read on any thread)
///
/// - NFS layers over one (`OverlayView<V, Overlay>`) = one too
pub trait PinnedView: SequenceRead + MapRead + 'static {}

/// One sequence table of one view
#[derive(Debug, Clone, Copy)]
pub struct SequenceView<'a, V> {
    view: &'a V,
    table: SequenceId,
}

impl<V: SequenceRead> SequenceView<'_, V> {
    /// Records held (= the next position)
    pub fn len(&self) -> u64 {
        self.view.len(self.table)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `None` = at or past `len`
    pub fn record(&self, at: u64) -> Option<Bytes> {
        self.view.record(self.table, at)
    }

    /// In order (`range` within `len`)
    pub fn records(&self, range: Range<u64>) -> Vec<Bytes> {
        self.view.records(self.table, range)
    }
}

/// One map table of one view
#[derive(Debug, Clone, Copy)]
pub struct MapView<'a, V> {
    view: &'a V,
    table: MapId,
}

impl<V: MapRead> MapView<'_, V> {
    pub fn value(&self, key: &[u8]) -> Option<Bytes> {
        self.view.value(self.table, key)
    }

    /// One answer per key, in `keys` order
    pub fn values(&self, keys: &[&[u8]]) -> Vec<Option<Bytes>> {
        self.view.values(self.table, keys)
    }

    /// `(key, value)` for `start <= key < end`, in key order (`None` = more than `limit`)
    pub fn range(&self, start: &[u8], end: &[u8], limit: usize) -> Option<Vec<(Bytes, Bytes)>> {
        self.view.range(self.table, start, end, limit)
    }
}

/// What [`PersistenceEngine::verify`] found
///
/// - `heights` = tip height + 1 (0 = nothing durable)
/// - `units` = files, for the file engines
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verification {
    pub heights: u64,
    pub units: Vec<Checked>,
}

impl Verification {
    pub fn is_clean(&self) -> bool {
        self.units.iter().all(Checked::is_clean)
    }
}

/// Stored unit, checked
///
/// - `orphaned_bytes` = past what the commit claims (interrupted commit's tail: harmless)
/// - `lost` = shorter than durable
/// - `bad_sums` = checksum list != commit's
/// - `bad_pages` = pages whose bytes != their checksum
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Checked {
    pub(crate) name: String,
    pub(crate) committed_bytes: u64,
    pub orphaned_bytes: u64,
    pub lost: bool,
    pub bad_sums: bool,
    pub bad_pages: Vec<u64>,
}

impl Checked {
    pub(crate) fn is_clean(&self) -> bool {
        !self.lost && !self.bad_sums && self.bad_pages.is_empty()
    }
}
