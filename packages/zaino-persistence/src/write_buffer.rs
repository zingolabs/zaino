//! [`WriteBuffer`]: a store's applied, uncommitted [`BlockChanges`], concatenated per table
//!
//! - map rows unsorted (the LSM sorts each batch at commit)
//! - per map: row numbers by key (staged lookups + the held-key check)
//! - a removed key = a tombstone row, unless the buffer holds its insert: both cancel (neither
//!   reaches a segment; `docs/design/lsm-deletes.md`)

use std::hash::BuildHasher;

use bytes::Bytes;
use hashbrown::{DefaultHashBuilder, HashTable};
use zaino_primitives::types::{BlockRef, Height};

use crate::{
    overlay::{Entry, OverlayView, Uncommitted},
    port::{
        BlockChanges, Items, MapId, MapItems, MapTable, Schema, SequenceId, SequenceTable, Width,
    },
};

/// Committed view + its store's buffer, borrowed ([`Store::staged`](crate::Store::staged))
///
/// - borrow = none held across `apply` (compile error, never a buffer copy)
pub type StagedView<'a, V> = OverlayView<V, &'a WriteBuffer>;

#[derive(Debug, Clone)]
pub struct WriteBuffer {
    schema: Schema,
    first: Option<Height>,
    tip: Option<BlockRef>,
    sequences: Vec<Items>,
    maps: Vec<MapRows>,
    hasher: DefaultHashBuilder,
}

/// One map's rows in arrival order + their row numbers by key
///
/// - a tombstone row's value bytes = zeros (fixed width) / empty (variable)
#[derive(Debug, Clone, Default)]
struct MapRows {
    keys: Items,
    values: Items,
    kinds: Vec<Row>,
    by_key: HashTable<usize>,
}

/// What a buffered map row is
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Value,
    Tombstone,
    /// inserted, then removed by a later buffered block: written nowhere, still indexed (a third
    /// write of the key = bug, caught)
    Cancelled,
}

impl WriteBuffer {
    pub(crate) fn empty(schema: &Schema) -> Self {
        Self {
            schema: *schema,
            first: None,
            tip: None,
            sequences: vec![Items::default(); schema.sequences().len()],
            maps: vec![MapRows::default(); schema.maps().len()],
            hasher: DefaultHashBuilder::default(),
        }
    }

    pub(crate) fn tip(&self) -> Option<BlockRef> {
        self.tip
    }

    /// Panics before buffering anything: a map key held or inserted twice, removed twice, or
    /// removed after its removal here
    pub(crate) fn push(&mut self, changes: &BlockChanges) {
        self.assert_new_keys(changes);
        for (&table, held) in self.schema.sequences().iter().zip(&mut self.sequences) {
            held.extend(changes.sequence_items(table));
        }
        for (&table, rows) in self.schema.maps().iter().zip(&mut self.maps) {
            rows.append(table, changes.map_items(table), &self.hasher);
        }
        self.first.get_or_insert(changes.block().height);
        self.tip = Some(changes.block());
    }

    fn assert_new_keys(&self, changes: &BlockChanges) {
        changes.assert_distinct_keys();
        for (&table, rows) in self.schema.maps().iter().zip(&self.maps) {
            let name = table.name;
            let held = |key: &[u8]| rows.find(table, key, &self.hasher);
            let held_twice = changes.inserts(table).any(|(key, _)| held(key).is_some());
            assert!(!held_twice, "{name}: a map key held twice");
            let removed_twice = changes
                .removes(table)
                .any(|key| held(key).is_some_and(|row| rows.kinds[row] != Row::Value));
            assert!(!removed_twice, "{name}: a map key removed twice");
        }
    }

    /// `table`'s records, oldest first (commit)
    pub(crate) fn records(&self, table: SequenceTable) -> impl Iterator<Item = &[u8]> {
        self.sequences[usize::from(table.id.0)].items(table.record)
    }

    /// `table`'s rows, arrival order (commit: the LSM sorts them); `None` = a tombstone
    pub(crate) fn map_rows(&self, table: MapTable) -> Vec<(&[u8], Option<&[u8]>)> {
        let rows = &self.maps[usize::from(table.id.0)];
        let all = rows.keys.items(table.key).zip(rows.values.items(table.value)).zip(&rows.kinds);
        let written = all.filter_map(|((key, value), kind)| match kind {
            Row::Value => Some((key, Some(value))),
            Row::Tombstone => Some((key, None)),
            Row::Cancelled => None,
        });
        written.collect()
    }

    /// Record + row bytes held, every table
    pub(crate) fn item_bytes(&self) -> usize {
        let sequences: usize = self.sequences.iter().map(Items::item_bytes).sum();
        let maps = self.maps.iter().map(|rows| rows.keys.item_bytes() + rows.values.item_bytes());
        sequences + maps.sum::<usize>()
    }

    /// Heap held: every table's bytes + the key indexes (capacity)
    pub(crate) fn heap(&self) -> usize {
        let sequences: usize = self.sequences.iter().map(Items::heap).sum();
        let maps = self.maps.iter().map(|rows| {
            let kinds = rows.kinds.capacity() * size_of::<Row>();
            rows.keys.heap() + rows.values.heap() + kinds + rows.by_key.allocation_size()
        });
        sequences + maps.sum::<usize>()
    }

    fn sequence(&self, id: SequenceId) -> (SequenceTable, &Items) {
        let at = usize::from(id.0);
        let table = self.schema.sequences().get(at);
        let table = table.unwrap_or_else(|| panic!("{id:?} not in the schema"));
        (*table, &self.sequences[at])
    }

    fn map(&self, id: MapId) -> (MapTable, &MapRows) {
        let at = usize::from(id.0);
        let table = self.schema.maps().get(at);
        let table = table.unwrap_or_else(|| panic!("{id:?} not in the schema"));
        (*table, &self.maps[at])
    }
}

impl MapRows {
    /// Inserts as value rows; each removal cancels the buffered insert of its key, else becomes a
    /// tombstone row
    fn append(&mut self, table: MapTable, items: &MapItems, hasher: &DefaultHashBuilder) {
        let first = self.keys.len(table.key);
        self.keys.extend(&items.keys);
        self.values.extend(&items.values);
        self.kinds.resize(self.keys.len(table.key), Row::Value);
        self.index(table, first, hasher);
        for key in items.removed.items(table.key) {
            if let Some(row) = self.find(table, key, hasher) {
                assert_eq!(self.kinds[row], Row::Value, "{}: a map key removed twice", table.name);
                self.kinds[row] = Row::Cancelled;
                continue;
            }
            let first = self.keys.len(table.key);
            self.keys.extend_one(table.key, key);
            let zeros = match table.value {
                Width::Fixed(n) => vec![0; n.get() as usize],
                Width::Variable => Vec::new(),
            };
            self.values.extend_one(table.value, &zeros);
            self.kinds.push(Row::Tombstone);
            self.index(table, first, hasher);
        }
    }

    /// Rows from `first` on, indexed by key
    fn index(&mut self, table: MapTable, first: usize, hasher: &DefaultHashBuilder) {
        for row in first..self.keys.len(table.key) {
            let held = &self.keys;
            let hash = hasher.hash_one(key_at(held, table, row));
            self.by_key
                .insert_unique(hash, row, |&other| hasher.hash_one(key_at(held, table, other)));
        }
    }

    fn find(&self, table: MapTable, key: &[u8], hasher: &DefaultHashBuilder) -> Option<usize> {
        let same = |&row: &usize| self.keys.get(table.key, row) == Some(key);
        self.by_key.find(hasher.hash_one(key), same).copied()
    }
}

impl MapRows {
    /// Row `row` as seen above durable (a cancelled key = removed: its insert is buffered here,
    /// never durable)
    fn entry(&self, table: MapTable, row: usize) -> Entry {
        match self.kinds[row] {
            Row::Value => {
                let value = self.values.get(table.value, row).expect("a value row is buffered");
                Entry::Value(Bytes::copy_from_slice(value))
            }
            Row::Tombstone | Row::Cancelled => Entry::Removed,
        }
    }
}

fn key_at(keys: &Items, table: MapTable, row: usize) -> &[u8] {
    keys.get(table.key, row).expect("an indexed row is buffered")
}

#[cfg(test)]
impl WriteBuffer {
    /// Each map's index = its rows, each found by its own key; first <= tip
    pub(crate) fn check(&self, label: &str) {
        let first = self.first.map(|first| Some(first) <= self.tip.map(|tip| tip.height));
        assert!(first.is_none_or(|ordered| ordered), "{label}: buffer first above its tip");
        for (&table, rows) in self.schema.maps().iter().zip(&self.maps) {
            let count = rows.keys.len(table.key);
            let paired = rows.values.len(table.value) == count
                && rows.kinds.len() == count
                && rows.by_key.len() == count;
            assert!(paired, "{label}: {} keys, values, kinds and index agree", table.name);
            let indexed = (0..count).all(|row| {
                rows.find(table, key_at(&rows.keys, table, row), &self.hasher) == Some(row)
            });
            assert!(indexed, "{label}: {} every row found by its key", table.name);
        }
    }
}

impl Uncommitted for WriteBuffer {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn first(&self) -> Option<Height> {
        self.first
    }

    fn tip(&self) -> Option<BlockRef> {
        self.tip
    }

    fn record_count(&self, table: SequenceId) -> u64 {
        let (table, records) = self.sequence(table);
        records.len(table.record) as u64
    }

    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes> {
        let (table, records) = self.sequence(table);
        records.get(table.record, usize::try_from(at).ok()?).map(Bytes::copy_from_slice)
    }

    fn value(&self, table: MapId, key: &[u8]) -> Option<Entry> {
        let (table, rows) = self.map(table);
        let row = rows.find(table, key, &self.hasher)?;
        Some(rows.entry(table, row))
    }

    /// Full scan (no fold ranges over its own buffer: tests and tools only)
    fn rows(&self, table: MapId, start: &[u8], end: &[u8]) -> Vec<(Bytes, Entry)> {
        let (table, rows) = self.map(table);
        let keys = rows.keys.items(table.key).enumerate();
        let mut within: Vec<(usize, &[u8])> =
            keys.filter(|(_, key)| (start..end).contains(key)).collect();
        within.sort_unstable_by_key(|(_, key)| *key);
        within
            .into_iter()
            .map(|(row, key)| (Bytes::copy_from_slice(key), rows.entry(table, row)))
            .collect()
    }
}
