//! Store's declaration: the owner's name + tables ([`Schema`]), declared once as `const` handles
//!
//! - sequence table = records at positions 0, 1, 2, ...
//! - map table = values under unique keys

use std::{fmt, num::NonZeroUsize};

use zaino_primitives::types::BlockRef;

use crate::changes::BlockChanges;

/// Sequence table's position among its schema's sequences
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceId(pub(crate) usize);

/// Map table's position among its schema's maps
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MapId(pub(crate) usize);

impl From<SequenceId> for usize {
    fn from(id: SequenceId) -> Self {
        id.0
    }
}

impl From<MapId> for usize {
    fn from(id: MapId) -> Self {
        id.0
    }
}

/// `id`'s entry in a list held per table, schema order (panics: an id past the schema)
pub(crate) fn table_slot<Id: Into<usize> + Copy + fmt::Debug, T>(per_table: &[T], id: Id) -> &T {
    per_table.get(id.into()).unwrap_or_else(|| panic!("{id:?} not in the schema"))
}

/// Bytes per sequence record
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Fixed(NonZeroUsize),
    Variable,
}

impl From<NonZeroUsize> for Width {
    fn from(bytes: NonZeroUsize) -> Self {
        Self::Fixed(bytes)
    }
}

/// `n` bytes, `n` > 0 (checked at compile time when `const`)
const fn fixed_bytes(n: usize) -> NonZeroUsize {
    match NonZeroUsize::new(n) {
        Some(n) => n,
        None => panic!("a fixed width is at least one byte"),
    }
}

impl Width {
    /// `Fixed(n)` (`n` > 0, checked at compile time when `const`)
    pub const fn fixed(n: usize) -> Self {
        Self::Fixed(fixed_bytes(n))
    }
}

/// Handle + declaration: `name` = place in the store (`/` = sub-directory, for engines with them)
///
/// - `cache_writes` = appended data kept in page cache ([`cache_writes`](Self::cache_writes))
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceTable {
    pub(crate) id: SequenceId,
    pub name: &'static str,
    pub record: Width,
    pub(crate) cache_writes: bool,
}

impl SequenceTable {
    /// `id` = position in its [`Tables`]
    pub const fn new(id: usize, name: &'static str, record: Width) -> Self {
        Self { id: SequenceId(id), name, record, cache_writes: false }
    }

    /// Fold reads it: what a commit appends stays in page cache
    ///
    /// - default: dropped once synced (bulk appends never evict the tables folds read)
    /// - serving faults pages back in
    pub const fn cache_writes(self) -> Self {
        Self { cache_writes: true, ..self }
    }
}

/// Handle + declaration, keys compare as bytes (big-endian fields = numeric order)
///
/// - `scope` = leading key bytes every range read shares, partition hint
///   (0 = [`point`](Self::point))
/// - keys lead with >= 8 uniform bytes (hash, txid: shardable)
/// - `key`, `value` = fixed widths in bytes (rows = fixed stride)
/// - `cache_writes` = written data kept in page cache ([`cache_writes`](Self::cache_writes))
/// - `deletes` = keys removable ([`deletes`](Self::deletes))
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapTable {
    pub(crate) id: MapId,
    pub(crate) name: &'static str,
    pub key: NonZeroUsize,
    pub(crate) value: NonZeroUsize,
    pub(crate) scope: usize,
    pub(crate) cache_writes: bool,
    pub(crate) deletes: bool,
}

impl MapTable {
    /// Point lookups only (scope 0)
    ///
    /// - `id` = position in its [`Tables`]
    /// - `key`, `value` = bytes (> 0, checked at compile time when `const`)
    pub const fn point(id: usize, name: &'static str, key: usize, value: usize) -> Self {
        let (key, value) = (fixed_bytes(key), fixed_bytes(value));
        Self { id: MapId(id), name, key, value, scope: 0, cache_writes: false, deletes: false }
    }

    /// Range reads within `scope` leading key bytes (panics: `scope` 0, checked at compile time
    /// when `const`)
    pub const fn scoped(
        id: usize,
        name: &'static str,
        key: usize,
        value: usize,
        scope: usize,
    ) -> Self {
        assert!(scope > 0, "a scoped map shares at least one key byte per range");
        Self { scope, ..Self::point(id, name, key, value) }
    }

    /// Keys removable ([`MapChanges::remove`]) under a contract: per key, inserted at most once,
    /// removed at most once, never re-inserted (RocksDB `SingleDelete`, usage.md "Deletes")
    ///
    /// - the LSM then stores a flag byte per row (value or tombstone)
    ///
    /// [`MapChanges::remove`]: crate::MapChanges::remove
    pub const fn deletes(self) -> Self {
        Self { deletes: true, ..self }
    }

    /// Fold reads it: what a commit or merge writes stays in page cache
    ///
    /// - default: dropped once synced (bulk writes never evict the tables folds read)
    /// - serving faults pages back in
    pub const fn cache_writes(self) -> Self {
        Self { cache_writes: true, ..self }
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
            assert!(sequences[at].id.0 == at, "sequence id != its position");
            at += 1;
        }
        let mut at = 0;
        while at < maps.len() {
            assert!(maps[at].id.0 == at, "map id != its position");
            at += 1;
        }
        Self { sequences, maps }
    }
}

/// What a store holds + whose: declared at open, same every time
///
/// - `name` = metric / log label + the directory's identity
/// - `format` = record layout version, `scope` = the owner's namespace (a directory opened under
///   another name, format or scope = refused)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schema {
    name: &'static str,
    pub(crate) format: u16,
    scope: &'static str,
    tables: Tables,
}

impl Schema {
    pub const fn new(name: &'static str, format: u16, scope: &'static str, tables: Tables) -> Self {
        assert!(name.len() <= 255 && scope.len() <= 255, "name, scope: <= 255 bytes each");
        Self { name, format, scope, tables }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn scope(&self) -> &'static str {
        self.scope
    }

    pub fn sequences(&self) -> &'static [SequenceTable] {
        self.tables.sequences
    }

    pub fn maps(&self) -> &'static [MapTable] {
        self.tables.maps
    }

    /// Empty delta for block `at`, one buffer per table
    pub fn changes(&self, at: BlockRef) -> BlockChanges {
        BlockChanges::new(at, *self)
    }

    /// `table`'s position (panics: another schema's)
    pub(crate) fn sequence_at(&self, table: SequenceTable) -> usize {
        let at = table.id.0;
        let ours = self.tables.sequences.get(at) == Some(&table);
        assert!(ours, "{}: sequence {} not in its schema", self.name, table.name);
        at
    }

    /// `table`'s position (panics: another schema's)
    pub(crate) fn map_at(&self, table: MapTable) -> usize {
        let at = table.id.0;
        let ours = self.tables.maps.get(at) == Some(&table);
        assert!(ours, "{}: map {} not in its schema", self.name, table.name);
        at
    }
}
