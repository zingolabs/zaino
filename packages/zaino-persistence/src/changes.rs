//! One block's delta ([`BlockChanges`]): per-table appends, inserts, removes

use zaino_primitives::types::BlockRef;

use crate::schema::{MapTable, Schema, SequenceTable, Width};

/// One block's delta: buffer per table, opened by [`Schema::changes`]
///
/// - widths checked per item on arrival (wrong = panic naming the table)
/// - fixed-width tables: bytes only
/// - variable: + end offset per item
#[derive(Debug, Clone)]
pub struct BlockChanges {
    block: BlockRef,
    schema: Schema,
    sequences: Vec<Items>,
    maps: Vec<MapItems>,
}

/// One map's share of a [`BlockChanges`]: inserted `(keys, values)` + removed keys
#[derive(Debug, Clone, Default)]
pub(crate) struct MapItems {
    pub(crate) keys: Items,
    pub(crate) values: Items,
    pub(crate) removed: Items,
}

impl BlockChanges {
    pub(crate) fn new(block: BlockRef, schema: Schema) -> Self {
        Self {
            block,
            schema,
            sequences: vec![Items::default(); schema.sequences().len()],
            maps: vec![MapItems::default(); schema.maps().len()],
        }
    }

    /// Block these changes are for (applied = the store's tip)
    pub fn block(&self) -> BlockRef {
        self.block
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// `table`'s appends (panics: not in this schema)
    pub fn sequence(&mut self, table: SequenceTable) -> SequenceAppends<'_> {
        let at = self.schema.sequence_at(table);
        SequenceAppends { table, buffer: &mut self.sequences[at] }
    }

    /// `table`'s inserts + removes (panics: not in this schema)
    pub fn map(&mut self, table: MapTable) -> MapChanges<'_> {
        let at = self.schema.map_at(table);
        MapChanges { table, items: &mut self.maps[at] }
    }

    /// Records appended + rows inserted + keys removed, every table
    pub fn rows(&self) -> usize {
        let schema = self.schema;
        let records =
            schema.sequences().iter().map(|&table| self.sequence_items(table).len(table.record));
        let rows = schema.maps().iter().map(|&table| {
            let items = self.map_items(table);
            items.keys.len(table.key) + items.removed.len(table.key)
        });
        records.chain(rows).sum()
    }

    /// `table`'s appends, in the order made
    pub fn appends(&self, table: SequenceTable) -> impl Iterator<Item = &[u8]> {
        self.sequences[self.schema.sequence_at(table)].items(table.record)
    }

    /// `table`'s inserts as `(key, value)`, in the order made
    pub fn inserts(&self, table: MapTable) -> impl Iterator<Item = (&[u8], &[u8])> {
        let items = &self.maps[self.schema.map_at(table)];
        items.keys.items(table.key).zip(items.values.items(table.value))
    }

    /// `table`'s removed keys, in the order made
    pub fn removes(&self, table: MapTable) -> impl Iterator<Item = &[u8]> {
        self.maps[self.schema.map_at(table)].removed.items(table.key)
    }

    pub(crate) fn sequence_items(&self, table: SequenceTable) -> &Items {
        &self.sequences[self.schema.sequence_at(table)]
    }

    pub(crate) fn map_items(&self, table: MapTable) -> &MapItems {
        &self.maps[self.schema.map_at(table)]
    }

    /// Panics before anything is applied: a key inserted twice, removed twice, or both inserted
    /// and removed in these changes (a block creating and spending a key writes neither)
    pub(crate) fn assert_distinct_keys(&self) {
        for (&table, items) in self.schema.maps().iter().zip(&self.maps) {
            let name = table.name;
            let mut inserted: Vec<&[u8]> = items.keys.items(table.key).collect();
            let mut removed: Vec<&[u8]> = items.removed.items(table.key).collect();
            inserted.sort_unstable();
            removed.sort_unstable();
            let twice = |keys: &[&[u8]]| keys.windows(2).any(|pair| pair[0] == pair[1]);
            assert!(!twice(&inserted), "{name}: a map key inserted twice");
            assert!(!twice(&removed), "{name}: a map key removed twice");
            let both = removed.iter().any(|key| inserted.binary_search(key).is_ok());
            assert!(!both, "{name}: a map key inserted and removed by one block");
        }
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

/// One map table's inserts + removes in a [`BlockChanges`]
#[derive(Debug)]
pub struct MapChanges<'a> {
    table: MapTable,
    items: &'a mut MapItems,
}

impl MapChanges<'_> {
    /// `value` under `key` (keys unique: second insert = bug, caught at apply)
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        self.items.keys.push(self.table.name, self.table.key, key);
        self.items.values.push(self.table.name, self.table.value, value);
    }

    /// `key` gone from this block on (insert-once / remove-once: a second remove, a remove of a
    /// key inserted here, or a re-insert = bug, caught at apply)
    ///
    /// - panics: the table does not declare [`MapTable::deletes`]
    pub fn remove(&mut self, key: &[u8]) {
        let name = self.table.name;
        assert!(self.table.deletes, "{name}: remove on a table without `deletes()`");
        self.items.removed.push(name, self.table.key, key);
    }
}

/// Table's items back to back (`ends` = where each ends, Variable only)
#[derive(Debug, Clone, Default)]
pub(crate) struct Items {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl Items {
    fn push(&mut self, table: &str, width: impl Into<Width>, item: &[u8]) {
        match width.into() {
            Width::Fixed(n) => {
                let len = item.len();
                assert_eq!(len, n.get(), "{table}: a {len}-byte item, width {n}");
            }
            Width::Variable => self.ends.push(self.bytes.len() + item.len()),
        }
        self.bytes.extend_from_slice(item);
    }

    pub(crate) fn len(&self, width: impl Into<Width>) -> usize {
        match width.into() {
            Width::Fixed(n) => self.bytes.len() / n,
            Width::Variable => self.ends.len(),
        }
    }

    pub(crate) fn get(&self, width: impl Into<Width>, at: usize) -> Option<&[u8]> {
        let width = width.into();
        if at >= self.len(width) {
            return None;
        }
        let range = match width {
            Width::Fixed(n) => at * n.get()..(at + 1) * n.get(),
            Width::Variable => {
                at.checked_sub(1).map_or(0, |before| self.ends[before])..self.ends[at]
            }
        };
        Some(&self.bytes[range])
    }

    pub(crate) fn items(&self, width: impl Into<Width>) -> impl Iterator<Item = &[u8]> {
        let width = width.into();
        (0..self.len(width)).filter_map(move |at| self.get(width, at))
    }

    /// `other`'s items after these (one copy, its end offsets shifted)
    pub(crate) fn extend(&mut self, other: &Items) {
        let base = self.bytes.len();
        self.ends.extend(other.ends.iter().map(|end| base + end));
        self.bytes.extend_from_slice(&other.bytes);
    }

    /// Item bytes held (end offsets not counted)
    pub(crate) fn item_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// `item` after these (one item)
    pub(crate) fn extend_one(&mut self, width: impl Into<Width>, item: &[u8]) {
        self.push("", width, item);
    }

    /// Heap held (capacity, not length)
    pub(crate) fn heap(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * size_of::<usize>()
    }
}
