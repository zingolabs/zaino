//! Non-final data over a committed view (`docs/design/nfs.md` §4)
//!
//! - [`Overlay`] = `BlockChanges` above some durable tip, per table the items they add (`imbl`)
//! - [`OverlayView`] = rows [`Uncommitted`] the committed view they sit on: above first, then durable

use std::{
    fmt,
    ops::{Bound, Range},
    sync::Arc,
};

use bytes::Bytes;
use imbl::{OrdMap, Vector};
use zaino_primitives::types::{BlockRef, Height};

use crate::port::{
    BlockChanges, CommittedView, MapId, MapRead, Schema, SequenceId, SequenceRead, View,
};

/// Index's data above a durable tip, as of one block (clone = O(tables) pointer copies)
///
/// - `deltas` = each `BlockChanges` absorbed, oldest first ([`rebase`](Self::rebase) drops by them)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    schema: Schema,
    deltas: Vector<Arc<Delta>>,
    sequences: Vec<Vector<Bytes>>,
    maps: Vec<OrdMap<Bytes, Bytes>>,
}

/// `BlockChanges`' share: `(sequence, records)` per sequence it grew (sparse), keys per map
#[derive(Debug, PartialEq, Eq)]
struct Delta {
    tip: BlockRef,
    appends: Vec<(usize, usize)>,
    keys: Vec<Vec<Bytes>>,
}

impl Overlay {
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
    pub fn changes(&self, at: BlockRef) -> BlockChanges {
        BlockChanges::new(at, self.schema)
    }

    /// This layer + `changes`, sharing structure with `self`
    ///
    /// - panics: tip not above this one, another schema's tables, map key held twice
    pub fn with(&self, changes: &BlockChanges) -> Self {
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

    /// [`with`](Self::with) in place
    fn push(&mut self, changes: &BlockChanges) {
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
    fn assert_new_keys(&self, changes: &BlockChanges) {
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

/// Rows above a committed view: [`Overlay`] (NFS snapshots) or a store's uncommitted
/// [`WriteBuffer`](crate::write_buffer::WriteBuffer)
pub trait Uncommitted: Clone {
    fn schema(&self) -> &Schema;

    /// Lowest block held (`None` = empty)
    fn first(&self) -> Option<Height>;

    fn tip(&self) -> Option<BlockRef>;

    fn record_count(&self, table: SequenceId) -> u64;

    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes>;

    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes>;

    /// Up to `limit` rows in `start..end`, key order
    fn rows(&self, table: MapId, start: &[u8], end: &[u8], limit: usize) -> Vec<(Bytes, Bytes)>;
}

impl Uncommitted for Overlay {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn first(&self) -> Option<Height> {
        self.deltas.front().map(|delta| delta.tip.height)
    }

    fn tip(&self) -> Option<BlockRef> {
        Overlay::tip(self)
    }

    fn record_count(&self, table: SequenceId) -> u64 {
        self.records(table).len() as u64
    }

    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes> {
        self.records(table).get(at as usize).cloned()
    }

    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes> {
        self.rows(table).get(key).cloned()
    }

    fn rows(&self, table: MapId, start: &[u8], end: &[u8], limit: usize) -> Vec<(Bytes, Bytes)> {
        let bounds = (Bound::Included(start), Bound::Excluded(end));
        let rows = Overlay::rows(self, table).range::<_, [u8]>(bounds).take(limit);
        rows.map(|(key, value)| (key.clone(), value.clone())).collect()
    }
}

impl<A: Uncommitted> Uncommitted for &A {
    fn schema(&self) -> &Schema {
        (**self).schema()
    }

    fn first(&self) -> Option<Height> {
        (**self).first()
    }

    fn tip(&self) -> Option<BlockRef> {
        (**self).tip()
    }

    fn record_count(&self, table: SequenceId) -> u64 {
        (**self).record_count(table)
    }

    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes> {
        (**self).record(table, at)
    }

    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes> {
        (**self).value(table, key)
    }

    fn rows(&self, table: MapId, start: &[u8], end: &[u8], limit: usize) -> Vec<(Bytes, Bytes)> {
        (**self).rows(table, start, end, limit)
    }
}

/// Rows above over the committed view they sit on: one state, fixed while held
#[derive(Clone)]
pub struct OverlayView<V, A = Overlay> {
    durable: V,
    above: A,
}

impl<V: View, A: Uncommitted> OverlayView<V, A> {
    /// Panics: `above` not above `durable`'s tip (rebase it first), or of another schema
    pub fn new(durable: V, above: A) -> Self {
        assert_eq!(above.schema(), durable.schema(), "layer over another schema's view");
        let first = above.first();
        let floor = durable.tip().map(|tip| tip.height);
        let is_above = first.is_none_or(|first| Some(first) > floor);
        assert!(is_above, "layer from {first:?} not above durable {floor:?}: rebase it first");
        Self { durable, above }
    }

    /// Committed state underneath (the seam: at or below its tip = durable)
    pub fn durable(&self) -> &V {
        &self.durable
    }
}

impl<V: View, A: Uncommitted + Send + Sync> View for OverlayView<V, A> {
    fn tip(&self) -> Option<BlockRef> {
        self.above.tip().or_else(|| self.durable.tip())
    }

    fn schema(&self) -> &Schema {
        self.durable.schema()
    }
}

impl<V: View, A: Uncommitted> fmt::Debug for OverlayView<V, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OverlayView")
            .field("tip", &self.above.tip().or_else(|| self.durable.tip()))
            .field("durable", &self.durable.tip())
            .finish_non_exhaustive()
    }
}

impl<V: CommittedView> CommittedView for OverlayView<V, Overlay> {}

impl<V: SequenceRead, A: Uncommitted + Send + Sync> SequenceRead for OverlayView<V, A> {
    fn len(&self, table: SequenceId) -> u64 {
        self.durable.len(table) + self.above.record_count(table)
    }

    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes> {
        match at.checked_sub(self.durable.len(table)) {
            None => self.durable.record(table, at),
            Some(above) => self.above.record(table, above),
        }
    }

    fn records(&self, table: SequenceId, range: Range<u64>) -> Vec<Bytes> {
        let durable = self.durable.len(table);
        let below = range.start.min(durable)..range.end.min(durable);
        let mut records =
            if below.is_empty() { Vec::new() } else { self.durable.records(table, below) };
        let above = range.start.max(durable) - durable..range.end.max(durable) - durable;
        records.extend(above.map(|at| {
            let record = self.above.record(table, at);
            record.unwrap_or_else(|| panic!("{table:?}: records past len"))
        }));
        records
    }
}

impl<V: MapRead, A: Uncommitted + Send + Sync> MapRead for OverlayView<V, A> {
    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes> {
        self.above.value(table, key).or_else(|| self.durable.value(table, key))
    }

    fn values(&self, table: MapId, keys: &[&[u8]]) -> Vec<Option<Bytes>> {
        let mut answers: Vec<Option<Bytes>> =
            keys.iter().map(|key| self.above.value(table, key)).collect();
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
        let above = self.above.rows(table, start, end, left.saturating_add(1));
        if above.len() > left {
            return None;
        }
        rows.extend(above);
        // two sorted runs, no key in both → stable sort = one merge pass
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Some(rows)
    }
}

#[cfg(test)]
mod tests;
