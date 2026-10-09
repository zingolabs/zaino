//! [`WriteBuffer`]: a store's applied, uncommitted [`BlockChanges`], concatenated per table
//!
//! - map rows unsorted (the LSM sorts each batch at commit)
//! - per map: row numbers by key (staged lookups + the held-key check)

use std::hash::BuildHasher;

use bytes::Bytes;
use hashbrown::{DefaultHashBuilder, HashTable};
use zaino_primitives::types::{BlockRef, Height};

use crate::{
    overlay::{OverlayView, Uncommitted},
    port::{BlockChanges, Items, MapId, MapTable, Schema, SequenceId, SequenceTable},
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
#[derive(Debug, Clone, Default)]
struct MapRows {
    keys: Items,
    values: Items,
    by_key: HashTable<usize>,
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

    /// Panics before buffering anything: a map key held or inserted twice
    pub(crate) fn push(&mut self, changes: &BlockChanges) {
        self.assert_new_keys(changes);
        for (&table, held) in self.schema.sequences().iter().zip(&mut self.sequences) {
            held.extend(changes.sequence_items(table));
        }
        for (&table, rows) in self.schema.maps().iter().zip(&mut self.maps) {
            rows.append(table, changes.map_items(table), &self.hasher);
        }
        self.first.get_or_insert(changes.tip().height);
        self.tip = Some(changes.tip());
    }

    fn assert_new_keys(&self, changes: &BlockChanges) {
        for (&table, rows) in self.schema.maps().iter().zip(&self.maps) {
            let mut keys: Vec<&[u8]> = changes.inserts(table).map(|(key, _)| key).collect();
            keys.sort_unstable();
            let twice = keys.windows(2).any(|pair| pair[0] == pair[1])
                || keys.iter().any(|key| rows.find(table, key, &self.hasher).is_some());
            assert!(!twice, "{}: a map key held twice", table.name);
        }
    }

    /// `table`'s records, oldest first (commit)
    pub(crate) fn records(&self, table: SequenceTable) -> impl Iterator<Item = &[u8]> {
        self.sequences[usize::from(table.id.0)].items(table.record)
    }

    /// `table`'s rows, arrival order (commit: the LSM sorts them)
    pub(crate) fn map_rows(&self, table: MapTable) -> Vec<(&[u8], &[u8])> {
        let rows = &self.maps[usize::from(table.id.0)];
        rows.keys.items(table.key).zip(rows.values.items(table.value)).collect()
    }

    /// Heap held: every table's bytes + the key indexes (capacity)
    pub(crate) fn heap(&self) -> usize {
        let sequences: usize = self.sequences.iter().map(Items::heap).sum();
        let maps = self
            .maps
            .iter()
            .map(|rows| rows.keys.heap() + rows.values.heap() + rows.by_key.allocation_size());
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
    fn append(
        &mut self,
        table: MapTable,
        [keys, values]: &[Items; 2],
        hasher: &DefaultHashBuilder,
    ) {
        let first = self.keys.len(table.key);
        self.keys.extend(keys);
        self.values.extend(values);
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
            let paired = rows.values.len(table.value) == count && rows.by_key.len() == count;
            assert!(paired, "{label}: {} keys, values and index agree", table.name);
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

    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes> {
        let (table, rows) = self.map(table);
        let row = rows.find(table, key, &self.hasher)?;
        rows.values.get(table.value, row).map(Bytes::copy_from_slice)
    }

    /// Full scan (no fold ranges over its own buffer: tests and tools only)
    fn rows(&self, table: MapId, start: &[u8], end: &[u8], limit: usize) -> Vec<(Bytes, Bytes)> {
        let (table, rows) = self.map(table);
        let all = rows.keys.items(table.key).zip(rows.values.items(table.value));
        let mut within: Vec<(&[u8], &[u8])> =
            all.filter(|(key, _)| (start..end).contains(key)).collect();
        within.sort_unstable_by_key(|(key, _)| *key);
        let taken = within.into_iter().take(limit);
        taken
            .map(|(key, value)| (Bytes::copy_from_slice(key), Bytes::copy_from_slice(value)))
            .collect()
    }
}
