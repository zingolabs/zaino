//! Persistence port: what an index may ask of storage (`docs/design/persistence-engine.md`)
//!
//! - final data only, insert only, buffered then one atomic commit, snapshot reads, verifiable
//! - sequence table = records at positions 0, 1, 2, ...; map table = values under unique keys
//! - index declares its tables once (`const` handles); engine's `View` reads each kind it holds

use std::{num::NonZeroU32, ops::Range, path::Path};

use bytes::Bytes;
use serde::Serialize;
use zaino_primitives::types::{Block, BlockRef};
use zcash_protocol::consensus::NetworkType;

use crate::{manifest::IndexKind, write_buffer::StagedView, StoreError};

/// Storage backend: one store per index, verified offline
pub trait PersistenceEngine: Send + Sync + 'static {
    type Store: Store;

    /// Store at `path`: created, or resumed at its committed tip
    ///
    /// - another identity there (kind, format, network) = error, never a reformat
    /// - table this engine cannot hold = panic naming it (tables = constants: a bug)
    fn open(&self, path: &Path, schema: &Schema) -> Result<Self::Store, StoreError>;

    /// Every committed byte against its integrity data (read-only: safe beside a running writer)
    fn verify(&self, path: &Path, schema: &Schema) -> Result<Verification, StoreError>;
}

/// Index's store = commit point of all its tables (one writer)
///
/// - final data: [`apply`](Self::apply) buffers, [`commit`](Self::commit) makes it durable
pub trait Store: Send + 'static {
    type View: View;

    /// As opened
    fn schema(&self) -> &Schema;

    /// As opened (named by a failed commit's panic)
    fn path(&self) -> &Path;

    /// Empty delta for block `at`, one buffer per table of [`schema`](Self::schema)
    fn changes(&self, at: BlockRef) -> BlockChanges {
        BlockChanges::new(at, *self.schema())
    }

    /// `changes` buffered: in [`staged`](Self::staged), not in [`committed`](Self::committed),
    /// not durable
    ///
    /// - panics (nothing buffered): changes for another schema, a tip not above the last applied,
    ///   a map key the buffer already holds or `changes` inserts twice
    fn apply(&mut self, changes: BlockChanges);

    /// Heap the buffer holds (≈ RAM, >= its item bytes; a writer's batch trigger)
    fn buffered_bytes(&self) -> usize;

    /// Every buffered change + the last applied tip, durable together (one fsync), then in
    /// [`committed`](Self::committed)
    ///
    /// - nothing buffered = `Ok`, nothing written
    /// - `Err` poisons the store: every later commit panics (failed sync never retried)
    fn commit(&mut self) -> Result<(), StoreError>;

    /// Committed only (what serving pins: no crash takes back what a reader saw)
    fn committed(&self) -> Self::View;

    /// Committed + buffered (what a bulk fold reads its parent through)
    fn staged(&self) -> StagedView<Self::View>;
}

/// Committed state: fixed while held, shared by clones
pub trait View: Clone + Send + Sync + 'static {
    /// Last committed block (`None` = nothing committed yet)
    fn tip(&self) -> Option<BlockRef>;

    /// Store's, as opened
    fn schema(&self) -> &Schema;
}

/// Reads of sequence tables (engine side: by position; index side: [`sequence`](Self::sequence))
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

/// Reads of map tables (engine side: by position; index side: [`map`](Self::map))
pub trait MapRead: View {
    /// Value under `key`
    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes>;

    /// One answer per key, in `keys` order
    fn values(&self, table: MapId, keys: &[&[u8]]) -> Vec<Option<Bytes>>;

    /// `(key, value)` for `start <= key < end`, in key order; `None` = more than `limit`
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

/// One sequence table of one view
#[derive(Debug, Clone, Copy)]
pub struct SequenceView<'a, V> {
    view: &'a V,
    table: SequenceId,
}

impl<V: SequenceRead> SequenceView<'_, V> {
    /// Records held (= the next position)
    pub fn count(&self) -> u64 {
        self.view.len(self.table)
    }

    /// `None` = at or past `count`
    pub fn record(&self, at: u64) -> Option<Bytes> {
        self.view.record(self.table, at)
    }

    /// In order (`range` within `count`)
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

    /// `(key, value)` for `start <= key < end`, in key order; `None` = more than `limit`
    pub fn range(&self, start: &[u8], end: &[u8], limit: usize) -> Option<Vec<(Bytes, Bytes)>> {
        self.view.range(self.table, start, end, limit)
    }
}

/// Sequence table's position among its schema's sequences
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceId(pub(crate) u16);

/// Map table's position among its schema's maps
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MapId(pub(crate) u16);

/// Bytes per record, key or value
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Fixed(NonZeroU32),
    Variable,
}

impl Width {
    /// `Fixed(n)` (`n` > 0, checked at compile time when `const`)
    pub const fn fixed(n: u32) -> Self {
        match NonZeroU32::new(n) {
            Some(n) => Self::Fixed(n),
            None => panic!("a fixed width is at least one byte"),
        }
    }
}

/// Handle + declaration: `name` = place in the store (`/` = sub-directory, for engines with them)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceTable {
    pub(crate) id: SequenceId,
    pub name: &'static str,
    pub record: Width,
}

impl SequenceTable {
    /// `id` = position in its [`Tables`]
    pub const fn new(id: u16, name: &'static str, record: Width) -> Self {
        Self { id: SequenceId(id), name, record }
    }
}

/// Handle + declaration; keys compare as bytes (big-endian fields = numeric order)
///
/// - `scope` = leading key bytes every range read shares (0 = keys read whole; partition hint)
/// - keys lead with >= 8 uniform bytes (hash, txid: shardable)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapTable {
    pub(crate) id: MapId,
    pub(crate) name: &'static str,
    pub key: Width,
    pub(crate) value: Width,
    pub(crate) scope: u32,
}

impl MapTable {
    /// `id` = position in its [`Tables`]
    pub const fn new(id: u16, name: &'static str, key: Width, value: Width, scope: u32) -> Self {
        Self { id: MapId(id), name, key, value, scope }
    }
}

/// One index's tables, declared once (`const`: positions checked at compile time)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tables {
    sequences: &'static [SequenceTable],
    maps: &'static [MapTable],
}

impl Tables {
    /// Panics: an id != its position
    pub const fn new(sequences: &'static [SequenceTable], maps: &'static [MapTable]) -> Self {
        let mut at = 0;
        while at < sequences.len() {
            assert!(sequences[at].id.0 as usize == at, "sequence id != its position");
            at += 1;
        }
        let mut at = 0;
        while at < maps.len() {
            assert!(maps[at].id.0 as usize == at, "map id != its position");
            at += 1;
        }
        Self { sequences, maps }
    }
}

/// What a store holds + whose: declared at open, same every time (`format` = record layout version)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schema {
    pub kind: IndexKind,
    pub(crate) format: u16,
    pub network: NetworkType,
    tables: Tables,
}

impl Schema {
    pub const fn new(kind: IndexKind, format: u16, network: NetworkType, tables: Tables) -> Self {
        Self { kind, format, network, tables }
    }

    pub fn sequences(&self) -> &'static [SequenceTable] {
        self.tables.sequences
    }

    pub fn maps(&self) -> &'static [MapTable] {
        self.tables.maps
    }

    /// `table`'s position (panics: another schema's)
    fn sequence_at(&self, table: SequenceTable) -> usize {
        let at = usize::from(table.id.0);
        let ours = self.tables.sequences.get(at) == Some(&table);
        assert!(ours, "{}: sequence {} not in its schema", self.kind.name(), table.name);
        at
    }

    /// `table`'s position (panics: another schema's)
    fn map_at(&self, table: MapTable) -> usize {
        let at = usize::from(table.id.0);
        let ours = self.tables.maps.get(at) == Some(&table);
        assert!(ours, "{}: map {} not in its schema", self.kind.name(), table.name);
        at
    }
}

/// One block's delta: buffer per table, opened by [`Store::changes`] or `Overlay::changes`
///
/// - widths checked per item on arrival (wrong = panic naming the table)
/// - fixed-width tables: bytes only; variable: + end offset per item
#[derive(Debug, Clone)]
pub struct BlockChanges {
    tip: BlockRef,
    schema: Schema,
    sequences: Vec<Items>,
    maps: Vec<[Items; 2]>,
}

impl BlockChanges {
    pub(crate) fn new(tip: BlockRef, schema: Schema) -> Self {
        Self {
            tip,
            schema,
            sequences: vec![Items::default(); schema.sequences().len()],
            maps: vec![[Items::default(), Items::default()]; schema.maps().len()],
        }
    }

    pub fn tip(&self) -> BlockRef {
        self.tip
    }

    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Fold's preconditions, panic naming the index: opened for `block`, `block` next above
    /// `parent` (`None` = empty parent: genesis)
    pub fn assert_next(&self, parent: Option<BlockRef>, block: &Block) {
        let (name, at) = (self.schema.kind.name(), block.at());
        let tip = self.tip;
        assert!(tip == at, "{name}: changes opened for another block ({tip:?}), folding {at:?}");
        let extends = block.header().extends(parent);
        assert!(extends, "{name}: block {at:?} does not extend the parent tip {parent:?}");
    }

    /// [`assert_next`](Self::assert_next) over a run: `out[i]` for `blocks[i]`, each block next
    /// above the one before it (the first above `parent`)
    pub fn assert_run(parent: Option<BlockRef>, blocks: &[&Block], out: &[BlockChanges]) {
        assert_eq!(blocks.len(), out.len(), "one delta per block of the run");
        let mut below = parent;
        for (block, out) in blocks.iter().zip(out) {
            out.assert_next(below, block);
            below = Some(block.at());
        }
    }

    /// `table`'s appends (panics: not in this schema)
    pub fn sequence(&mut self, table: SequenceTable) -> SequenceAppends<'_> {
        let at = self.schema.sequence_at(table);
        SequenceAppends { table, buffer: &mut self.sequences[at] }
    }

    /// `table`'s inserts (panics: not in this schema)
    pub fn map(&mut self, table: MapTable) -> MapInserts<'_> {
        let at = self.schema.map_at(table);
        let [keys, values] = &mut self.maps[at];
        MapInserts { table, keys, values }
    }

    /// Item bytes held, every table (end offsets not counted)
    pub fn bytes(&self) -> usize {
        let maps = self.maps.iter().flatten();
        self.sequences.iter().chain(maps).map(|buffer| buffer.bytes.len()).sum()
    }

    /// `table`'s appends, in the order made
    pub fn appends(&self, table: SequenceTable) -> impl Iterator<Item = &[u8]> {
        self.sequences[self.schema.sequence_at(table)].items(table.record)
    }

    /// `table`'s inserts as `(key, value)`, in the order made
    pub fn inserts(&self, table: MapTable) -> impl Iterator<Item = (&[u8], &[u8])> {
        let [keys, values] = &self.maps[self.schema.map_at(table)];
        keys.items(table.key).zip(values.items(table.value))
    }

    pub(crate) fn sequence_items(&self, table: SequenceTable) -> &Items {
        &self.sequences[self.schema.sequence_at(table)]
    }

    /// `[keys, values]`
    pub(crate) fn map_items(&self, table: MapTable) -> &[Items; 2] {
        &self.maps[self.schema.map_at(table)]
    }
}

/// One sequence table's appends in a [`BlockChanges`]
#[derive(Debug)]
pub struct SequenceAppends<'a> {
    table: SequenceTable,
    buffer: &'a mut Items,
}

impl SequenceAppends<'_> {
    /// `record` at the end of the table (after every earlier append to it)
    pub fn append(&mut self, record: &[u8]) {
        self.buffer.push(self.table.name, self.table.record, record);
    }
}

/// One map table's inserts in a [`BlockChanges`]
#[derive(Debug)]
pub struct MapInserts<'a> {
    table: MapTable,
    keys: &'a mut Items,
    values: &'a mut Items,
}

impl MapInserts<'_> {
    /// `value` under `key` (keys unique: second insert = bug, caught at apply)
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        self.keys.push(self.table.name, self.table.key, key);
        self.values.push(self.table.name, self.table.value, value);
    }
}

/// Table's items back to back; `ends` = where each ends (Variable only)
#[derive(Debug, Clone, Default)]
pub(crate) struct Items {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl Items {
    fn push(&mut self, table: &str, width: Width, item: &[u8]) {
        match width {
            Width::Fixed(n) => {
                let len = item.len();
                assert_eq!(len, n.get() as usize, "{table}: a {len}-byte item, width {n}");
            }
            Width::Variable => self.ends.push(self.bytes.len() + item.len()),
        }
        self.bytes.extend_from_slice(item);
    }

    pub(crate) fn len(&self, width: Width) -> usize {
        match width {
            Width::Fixed(n) => self.bytes.len() / n.get() as usize,
            Width::Variable => self.ends.len(),
        }
    }

    pub(crate) fn get(&self, width: Width, at: usize) -> Option<&[u8]> {
        if at >= self.len(width) {
            return None;
        }
        let range = match width {
            Width::Fixed(n) => at * n.get() as usize..(at + 1) * n.get() as usize,
            Width::Variable => {
                at.checked_sub(1).map_or(0, |before| self.ends[before])..self.ends[at]
            }
        };
        Some(&self.bytes[range])
    }

    pub(crate) fn items(&self, width: Width) -> impl Iterator<Item = &[u8]> {
        (0..self.len(width)).filter_map(move |at| self.get(width, at))
    }

    /// `other`'s items after these (one copy, its end offsets shifted)
    pub(crate) fn extend(&mut self, other: &Items) {
        let base = self.bytes.len();
        self.ends.extend(other.ends.iter().map(|end| base + end));
        self.bytes.extend_from_slice(&other.bytes);
    }

    /// Heap held (capacity, not length)
    pub(crate) fn heap(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * size_of::<usize>()
    }
}

/// What [`PersistenceEngine::verify`] found
///
/// - `heights` = tip height + 1 (0 = nothing committed); `units` = files, for the file engines
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
/// - `lost` = shorter than committed; `bad_sums` = checksum list != commit's
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
