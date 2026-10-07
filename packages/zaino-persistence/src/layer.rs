//! Non-final data over a committed view (`docs/design/nfs.md` §4)
//!
//! - [`Layer`] = `Changes` above some durable tip, per table the items they add (`imbl`)
//! - [`LayeredView`] = layer over the committed view it sits on: layer first, then durable

use std::{
    fmt,
    ops::{Bound, Range},
    sync::Arc,
};

use bytes::Bytes;
use imbl::{OrdMap, Vector};
use zaino_primitives::types::BlockRef;

use crate::port::{Changes, MapId, MapRead, Schema, SequenceId, SequenceRead, View};

/// Index's data above a durable tip, as of one block (clone = O(tables) pointer copies)
///
/// - `deltas` = each `Changes` absorbed, oldest first ([`rebase`](Self::rebase) drops by them)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    schema: Schema,
    deltas: Vector<Arc<Delta>>,
    sequences: Vec<Vector<Bytes>>,
    maps: Vec<OrdMap<Bytes, Bytes>>,
}

/// `Changes`' share: `(sequence, records)` per sequence it grew (sparse), keys per map
#[derive(Debug, PartialEq, Eq)]
struct Delta {
    tip: BlockRef,
    appends: Vec<(usize, usize)>,
    keys: Vec<Vec<Bytes>>,
}

impl Layer {
    pub fn empty(schema: &Schema) -> Self {
        Self {
            schema: *schema,
            deltas: Vector::new(),
            sequences: vec![Vector::new(); schema.sequences().len()],
            maps: vec![OrdMap::new(); schema.maps().len()],
        }
    }

    /// Last block absorbed (`None` = empty: reads fall through to durable)
    pub fn tip(&self) -> Option<BlockRef> {
        self.deltas.last().map(|delta| delta.tip)
    }

    /// Empty delta for block `at`, shaped by this layer's schema (for [`with`](Self::with))
    pub fn changes(&self, at: BlockRef) -> Changes {
        Changes::new(at, self.schema)
    }

    /// This layer + `changes`, sharing structure with `self`
    ///
    /// - panics: tip not above this one, another schema's tables, map key held twice
    pub fn with(&self, changes: &Changes) -> Self {
        let mut next = self.clone();
        next.push(changes);
        next
    }

    /// What `durable` now holds dropped: every block through its tip
    ///
    /// - panics: `durable` past this layer's tip, or between two of its blocks (another branch)
    pub fn rebase(&self, durable: &impl View) -> Self {
        let Some(tip) = durable.tip() else { return self.clone() };
        let count = self.deltas.iter().take_while(|delta| delta.tip.height <= tip.height).count();
        let Some(last) = count.checked_sub(1).map(|at| self.deltas[at].tip) else {
            return self.clone();
        };
        assert_eq!(last, tip, "rebase onto a durable tip that is not one of the layer's blocks");

        let mut next = self.clone();
        let dropped = self.deltas.take(count);
        next.deltas = self.deltas.skip(count);
        let records = appended(dropped.iter(), self.sequences.len());
        for (held, records) in next.sequences.iter_mut().zip(records) {
            *held = held.skip(records);
        }
        for (at, held) in next.maps.iter_mut().enumerate() {
            for key in dropped.iter().flat_map(|delta| &delta.keys[at]) {
                held.remove(key);
            }
        }
        next
    }

    /// [`with`](Self::with) in place (store's buffer: unshared → no node copied)
    pub(crate) fn push(&mut self, changes: &Changes) {
        let (tip, last) = (changes.tip(), self.tip());
        let above = last.is_none_or(|last| tip.height > last.height);
        assert!(above, "layer: {tip:?} not above its tip {last:?}");
        assert_eq!(changes.schema(), &self.schema, "layer: another schema");
        self.assert_new_keys(changes);

        let appends = (self.schema.sequences().iter())
            .zip(&mut self.sequences)
            .enumerate()
            .filter_map(|(at, (&table, held))| {
                let before = held.len();
                held.extend(changes.appends(table).map(Bytes::copy_from_slice));
                (held.len() > before).then(|| (at, held.len() - before))
            })
            .collect();
        let keys = (self.schema.maps().iter())
            .zip(&mut self.maps)
            .map(|(&table, held)| {
                let rows = changes.inserts(table).map(|(key, value)| {
                    let key = Bytes::copy_from_slice(key);
                    held.insert(key.clone(), Bytes::copy_from_slice(value));
                    key
                });
                rows.collect()
            })
            .collect();
        self.deltas.push_back(Arc::new(Delta { tip, appends, keys }));
    }

    /// Panics: key `changes` inserts twice or this layer holds (before any state moves)
    fn assert_new_keys(&self, changes: &Changes) {
        for (&table, held) in self.schema.maps().iter().zip(&self.maps) {
            let mut keys: Vec<&[u8]> = changes.inserts(table).map(|(key, _)| key).collect();
            keys.sort_unstable();
            let twice = keys.windows(2).any(|pair| pair[0] == pair[1])
                || keys.iter().any(|key| held.contains_key(*key));
            assert!(!twice, "{}: a map key held twice", table.name);
        }
    }

    /// `table`'s records past durable, oldest first
    pub(crate) fn records(&self, table: SequenceId) -> &Vector<Bytes> {
        let found = self.sequences.get(usize::from(table.0));
        found.unwrap_or_else(|| panic!("{table:?} not in the schema"))
    }

    /// `table`'s rows above durable, in key order
    pub(crate) fn rows(&self, table: MapId) -> &OrdMap<Bytes, Bytes> {
        let found = self.maps.get(usize::from(table.0));
        found.unwrap_or_else(|| panic!("{table:?} not in the schema"))
    }

    /// Per-table items = what the blocks added; tips ascending
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn check(&self, label: &str) {
        let tips: Vec<_> = self.deltas.iter().map(|delta| delta.tip.height).collect();
        assert!(tips.is_sorted_by(|a, b| a < b), "{label}: layer blocks ascending {tips:?}");
        let records = appended(self.deltas.iter(), self.sequences.len());
        for (at, (held, records)) in self.sequences.iter().zip(records).enumerate() {
            assert_eq!(held.len(), records, "{label}: sequence {at} = its blocks' appends");
        }
        for (at, held) in self.maps.iter().enumerate() {
            let keys: Vec<&Bytes> = self.deltas.iter().flat_map(|delta| &delta.keys[at]).collect();
            let same = held.len() == keys.len() && keys.iter().all(|key| held.contains_key(*key));
            assert!(same, "{label}: map {at} = its blocks' keys");
        }
    }
}

/// Records `deltas` appended, per sequence (`tables` = the schema's sequence count)
fn appended<'a>(deltas: impl Iterator<Item = &'a Arc<Delta>>, tables: usize) -> Vec<usize> {
    let mut records = vec![0; tables];
    for &(table, count) in deltas.flat_map(|delta| &delta.appends) {
        records[table] += count;
    }
    records
}

/// Layer over the committed view it sits on: one state, fixed while held
#[derive(Clone)]
pub struct LayeredView<V> {
    durable: V,
    layer: Layer,
}

impl<V: View> LayeredView<V> {
    /// Panics: layer not above `durable`'s tip (rebase it first), or of another schema
    pub fn new(durable: V, layer: Layer) -> Self {
        assert_eq!(&layer.schema, durable.schema(), "layer over another schema's view");
        let first = layer.deltas.front().map(|delta| delta.tip.height);
        let floor = durable.tip().map(|tip| tip.height);
        let above = first.is_none_or(|first| Some(first) > floor);
        assert!(above, "layer from {first:?} not above durable {floor:?}: rebase it first");
        Self { durable, layer }
    }

    /// Committed state underneath (the seam: at or below its tip = durable)
    pub fn durable(&self) -> &V {
        &self.durable
    }
}

impl<V: View> View for LayeredView<V> {
    fn tip(&self) -> Option<BlockRef> {
        self.layer.tip().or_else(|| self.durable.tip())
    }

    fn schema(&self) -> &Schema {
        self.durable.schema()
    }
}

impl<V: View> fmt::Debug for LayeredView<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayeredView")
            .field("tip", &self.tip())
            .field("durable", &self.durable.tip())
            .finish_non_exhaustive()
    }
}

impl<V: SequenceRead> SequenceRead for LayeredView<V> {
    fn len(&self, table: SequenceId) -> u64 {
        self.durable.len(table) + self.layer.records(table).len() as u64
    }

    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes> {
        match at.checked_sub(self.durable.len(table)) {
            None => self.durable.record(table, at),
            Some(above) => self.layer.records(table).get(above as usize).cloned(),
        }
    }

    fn records(&self, table: SequenceId, range: Range<u64>) -> Vec<Bytes> {
        let durable = self.durable.len(table);
        let below = range.start.min(durable)..range.end.min(durable);
        let mut records =
            if below.is_empty() { Vec::new() } else { self.durable.records(table, below) };
        let layer = self.layer.records(table);
        let above = range.start.max(durable) - durable..range.end.max(durable) - durable;
        records.extend(above.map(|at| {
            let record = layer.get(at as usize).cloned();
            record.unwrap_or_else(|| panic!("{table:?}: records past len"))
        }));
        records
    }
}

impl<V: MapRead> MapRead for LayeredView<V> {
    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes> {
        let layer = self.layer.rows(table).get(key).cloned();
        layer.or_else(|| self.durable.value(table, key))
    }

    fn values(&self, table: MapId, keys: &[&[u8]]) -> Vec<Option<Bytes>> {
        let layer = self.layer.rows(table);
        let mut answers: Vec<Option<Bytes>> =
            keys.iter().map(|key| layer.get(*key).cloned()).collect();
        let misses: Vec<usize> = (0..keys.len()).filter(|&at| answers[at].is_none()).collect();
        let asked: Vec<&[u8]> = misses.iter().map(|&at| keys[at]).collect();
        for (at, found) in misses.into_iter().zip(self.durable.values(table, &asked)) {
            answers[at] = found;
        }
        answers
    }

    fn range(
        &self,
        table: MapId,
        start: &[u8],
        end: &[u8],
        limit: usize,
    ) -> Option<Vec<(Bytes, Bytes)>> {
        let mut rows = self.durable.range(table, start, end, limit)?;
        // inverted bounds panic `OrdMap::range`
        if start >= end {
            return Some(rows);
        }
        let left = limit - rows.len();
        let bounds = (Bound::Included(start), Bound::Excluded(end));
        let layer = self.layer.rows(table).range::<_, [u8]>(bounds).take(left.saturating_add(1));
        let before = rows.len();
        rows.extend(layer.map(|(key, value)| (key.clone(), value.clone())));
        if rows.len() - before > left {
            return None;
        }
        // two sorted runs, no key in both → stable sort = one merge pass
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Some(rows)
    }
}

#[cfg(test)]
mod tests;
