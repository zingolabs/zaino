//! The persistence port: what an index may ask of storage (`docs/design/persistence-engine.md`)
//!
//! - final data only, insert only, buffered then one atomic commit, snapshot reads, verifiable
//! - two kinds of table: a sequence (records at positions 0, 1, 2, ...) and a map (values under
//!   unique keys); an engine's `View` implements the read trait of each kind it can hold

use std::{num::NonZeroU32, ops::Range, path::Path};

use bytes::Bytes;
use serde::Serialize;
use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::NetworkType;

use crate::{layer::LayeredView, manifest::IndexKind, StoreError};

/// A storage backend: opens one store per index, verifies them offline
pub trait PersistenceEngine: Send + Sync + 'static {
    type Store: Store;

    /// Creates the store at `path`, or resumes it at its committed tip
    ///
    /// - another identity there (kind, format, network) = error, never a reformat
    /// - a table this engine cannot hold = panic naming the table (schemas = constants: a bug)
    fn open(&self, path: &Path, schema: &Schema) -> Result<Self::Store, StoreError>;

    /// Every committed byte against its integrity data (read-only: safe beside a running writer)
    fn verify(&self, path: &Path, schema: &Schema) -> Result<Verification, StoreError>;
}

/// One index's store: the commit point of all its tables (one writer)
///
/// - final data: [`apply`](Self::apply) buffers, [`commit`](Self::commit) makes it durable
pub trait Store: Send + 'static {
    type View: View;

    /// What it was opened with (what [`Changes::new`] shapes its buffers by)
    fn schema(&self) -> &Schema;

    /// Where it was opened (a failed commit's panic names it)
    fn path(&self) -> &Path;

    /// `changes` buffered: in [`staged`](Self::staged), not in [`view`](Self::view), not durable
    ///
    /// - panics (nothing buffered): changes for another schema, a tip not above the last applied,
    ///   a map key the buffer already holds or `changes` inserts twice
    fn apply(&mut self, changes: Changes);

    /// Item bytes buffered (a writer's batch trigger)
    fn buffered_bytes(&self) -> usize;

    /// Every buffered change + the last applied tip, durable together (one fsync), then in `view`
    ///
    /// - nothing buffered = `Ok`, nothing written
    /// - `Err` poisons the store: every later commit panics (a failed sync is never retried)
    fn commit(&mut self) -> Result<(), StoreError>;

    /// Committed only (what serving pins: a crash never takes back what a reader saw)
    fn view(&self) -> Self::View;

    /// Committed + buffered (what a bulk fold reads its parent through)
    fn staged(&self) -> LayeredView<Self::View>;
}

/// One committed state: never changes while held; a clone shares it
pub trait View: Clone + Send + Sync + 'static {
    /// Last committed block (`None` = nothing committed yet)
    fn tip(&self) -> Option<BlockRef>;
}

/// Reads of sequence tables
pub trait SequenceRead: View {
    /// Records held (= the next position)
    fn len(&self, table: SequenceId) -> u64;

    /// Record `at` (`None` = at or past `len`)
    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes>;

    /// Records in `range`, in order (`range` within `len`)
    fn records(&self, table: SequenceId, range: Range<u64>) -> Vec<Bytes>;
}

/// Reads of map tables
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
}

/// A sequence table's handle: its position among the schema's sequences
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceId(pub u16);

/// A map table's handle: its position among the schema's maps
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MapId(pub u16);

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

/// A sequence table: `name` = its place in the store (`/` = a sub-directory, where the engine
/// has directories)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceTable {
    pub name: String,
    pub record: Width,
}

/// A map table: keys compare as bytes (big-endian fields = numeric order)
///
/// - `scope` = leading key bytes every range read shares (0 = keys read whole); a hint some
///   engines index by (a partition key), others ignore
/// - keys lead with >= 8 uniform bytes (a hash, a txid): an engine may shard on them
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapTable {
    pub name: String,
    pub key: Width,
    pub value: Width,
    pub scope: u32,
}

/// What a store holds and whose it is: declared at open, the same every time
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub kind: IndexKind,
    /// Layout version of the index's records
    pub format: u16,
    pub network: NetworkType,
    pub sequences: Vec<SequenceTable>,
    pub maps: Vec<MapTable>,
}

impl Schema {
    pub fn new(kind: IndexKind, format: u16, network: NetworkType) -> Self {
        Self { kind, format, network, sequences: Vec::new(), maps: Vec::new() }
    }

    /// Declares sequence `id` (ids declared in order: 0, 1, 2, ...)
    pub fn with_sequence(mut self, id: SequenceId, name: &str, record: Width) -> Self {
        assert_eq!(usize::from(id.0), self.sequences.len(), "sequence {name}: ids in order");
        self.sequences.push(SequenceTable { name: name.to_owned(), record });
        self
    }

    /// Declares map `id` (ids declared in order: 0, 1, 2, ...)
    pub fn with_map(mut self, id: MapId, name: &str, key: Width, value: Width, scope: u32) -> Self {
        assert_eq!(usize::from(id.0), self.maps.len(), "map {name}: ids in order");
        self.maps.push(MapTable { name: name.to_owned(), key, value, scope });
        self
    }

    /// Panics on an id this schema never declared
    pub fn sequence(&self, id: SequenceId) -> &SequenceTable {
        let found = self.sequences.get(usize::from(id.0));
        found.unwrap_or_else(|| panic!("{id:?} not in the {:?} schema", self.kind))
    }

    /// Panics on an id this schema never declared
    pub fn map(&self, id: MapId) -> &MapTable {
        let found = self.maps.get(usize::from(id.0));
        found.unwrap_or_else(|| panic!("{id:?} not in the {:?} schema", self.kind))
    }

    pub fn sequence_ids(&self) -> impl Iterator<Item = SequenceId> {
        (0..self.sequences.len()).map(|at| SequenceId(u16::try_from(at).expect("ids are u16")))
    }

    pub fn map_ids(&self) -> impl Iterator<Item = MapId> {
        (0..self.maps.len()).map(|at| MapId(u16::try_from(at).expect("ids are u16")))
    }
}

/// One commit's worth of changes: one buffer per table, shaped by the schema
///
/// - widths checked as each item arrives (a wrong one = a panic naming the table)
/// - fixed-width tables hold their bytes only; variable ones add an end offset per item
#[derive(Debug, Clone)]
pub struct Changes {
    tip: BlockRef,
    schema: Schema,
    sequences: Vec<Buffer>,
    maps: Vec<[Buffer; 2]>,
}

impl Changes {
    /// Changes to a store of `schema`, committing through `tip`
    pub fn new(tip: BlockRef, schema: &Schema) -> Self {
        Self {
            tip,
            schema: schema.clone(),
            sequences: vec![Buffer::default(); schema.sequences.len()],
            maps: vec![[Buffer::default(), Buffer::default()]; schema.maps.len()],
        }
    }

    /// `record` at the end of `table` (after every earlier append to it)
    pub fn append(&mut self, table: SequenceId, record: &[u8]) {
        let SequenceTable { name, record: width } = self.schema.sequence(table);
        self.sequences[usize::from(table.0)].push(name, *width, record);
    }

    /// `value` under `key` in `table` (keys unique: inserting one twice is a bug)
    pub fn insert(&mut self, table: MapId, key: &[u8], value: &[u8]) {
        let MapTable { name, key: key_width, value: value_width, .. } = self.schema.map(table);
        let [keys, values] = &mut self.maps[usize::from(table.0)];
        keys.push(name, *key_width, key);
        values.push(name, *value_width, value);
    }

    pub fn tip(&self) -> BlockRef {
        self.tip
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Item bytes held, every table (end offsets not counted)
    pub fn bytes(&self) -> usize {
        let maps = self.maps.iter().flatten();
        self.sequences.iter().chain(maps).map(|buffer| buffer.bytes.len()).sum()
    }

    /// `table`'s appends, in the order made
    pub fn appends(&self, table: SequenceId) -> impl Iterator<Item = &[u8]> {
        let width = self.schema.sequence(table).record;
        self.sequences[usize::from(table.0)].items(width)
    }

    /// `table`'s inserts as `(key, value)`, in the order made
    pub fn inserts(&self, table: MapId) -> impl Iterator<Item = (&[u8], &[u8])> {
        let MapTable { key, value, .. } = self.schema.map(table);
        let [keys, values] = &self.maps[usize::from(table.0)];
        keys.items(*key).zip(values.items(*value))
    }
}

/// One table's items back to back; `ends` = where each ends (Variable only)
#[derive(Debug, Clone, Default)]
struct Buffer {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl Buffer {
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

    fn items(&self, width: Width) -> impl Iterator<Item = &[u8]> {
        let count = match width {
            Width::Fixed(n) => self.bytes.len() / n.get() as usize,
            Width::Variable => self.ends.len(),
        };
        (0..count).map(move |at| match width {
            Width::Fixed(n) => {
                let n = n.get() as usize;
                &self.bytes[at * n..(at + 1) * n]
            }
            Width::Variable => {
                let start = at.checked_sub(1).map_or(0, |before| self.ends[before]);
                &self.bytes[start..self.ends[at]]
            }
        })
    }
}

/// What [`PersistenceEngine::verify`] found
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verification {
    /// Blocks committed from genesis (tip height + 1, 0 = nothing committed)
    pub heights: u64,
    /// Each stored unit checked (a file, for the file engines)
    pub units: Vec<Checked>,
}

impl Verification {
    pub fn is_clean(&self) -> bool {
        self.units.iter().all(Checked::is_clean)
    }
}

/// One stored unit, checked
///
/// - `orphaned_bytes` = past what the commit claims (an interrupted commit's tail: harmless)
/// - `lost` = shorter than committed; `bad_sums` = its checksum list does not match the commit;
///   `bad_pages` = pages whose bytes do not match their checksum
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Checked {
    pub name: String,
    pub committed_bytes: u64,
    pub orphaned_bytes: u64,
    pub lost: bool,
    pub bad_sums: bool,
    pub bad_pages: Vec<u64>,
}

impl Checked {
    pub fn is_clean(&self) -> bool {
        !self.lost && !self.bad_sums && self.bad_pages.is_empty()
    }
}
