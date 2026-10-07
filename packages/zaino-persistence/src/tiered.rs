//! Blocks above a store's committed tip, read over its durable view (`persistence-engine.md` §5)
//!
//! - one block = one [`Changes`] (its tip = that block), held keyed exactly as the store holds it
//! - staged = final blocks batched by source bytes into one commit; applied = reorgable tip blocks
//! - never both at once (a tip block builds on durable: staged blocks commit first)
//! - `imbl` per table: a view = O(tables) pointer copies (published once per block)

use std::{
    fmt,
    num::NonZeroUsize,
    ops::{Bound, Range},
    sync::Arc,
};

use bytes::Bytes;
use imbl::{OrdMap, Vector};
use zaino_primitives::types::{BlockRef, Height};

use crate::port::{Changes, MapId, MapRead, Schema, SequenceId, SequenceRead, Store, View};

/// One index's store + the blocks held above its committed tip (one writer)
///
/// - `staged` = source bytes of the held blocks when final (`None` = held blocks applied, or none)
pub struct Tiered<S: Store> {
    store: S,
    batch: NonZeroUsize,
    staged: Option<usize>,
    view: TieredView<S::View>,
}

/// Held blocks first, then the durable view: one state, never changes while held
#[derive(Clone)]
pub struct TieredView<V> {
    durable: V,
    held: Held,
}

/// Blocks above durable, oldest first + per table the items they add past durable's
#[derive(Clone)]
struct Held {
    blocks: Vector<Arc<HeldBlock>>,
    sequences: Vec<Vector<Bytes>>,
    maps: Vec<OrdMap<Bytes, Bytes>>,
}

/// `(sequence, records)` per sequence it grew (sparse: most blocks touch few), keys per map
struct HeldBlock {
    tip: BlockRef,
    appends: Vec<(usize, usize)>,
    keys: Vec<Vec<Bytes>>,
}

/// Records `blocks` appended, per sequence (`tables` = the schema's sequence count)
fn appended<'a>(blocks: impl Iterator<Item = &'a Arc<HeldBlock>>, tables: usize) -> Vec<usize> {
    let mut records = vec![0; tables];
    for &(table, count) in blocks.flat_map(|block| &block.appends) {
        records[table] += count;
    }
    records
}

impl<S: Store> Tiered<S> {
    /// Nothing held above `store`'s committed tip; `batch` = source bytes per staged commit
    pub fn new(store: S, batch: NonZeroUsize) -> Self {
        let view = TieredView { held: Held::empty(store.schema()), durable: store.view() };
        Self { store, batch, staged: None, view }
    }

    pub fn schema(&self) -> &Schema {
        self.store.schema()
    }

    /// Everything held over everything committed (a clone per publication)
    pub fn view(&self) -> TieredView<S::View> {
        self.view.clone()
    }

    /// Last committed block (`None` = nothing committed)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.view.durable.tip()
    }

    /// Last applied block, else the durable tip (staged = final, never applied)
    pub fn applied(&self) -> Option<BlockRef> {
        match self.staged {
            Some(_) => self.durable_tip(),
            None => self.view.tip(),
        }
    }

    /// Last staged block (`None` = nothing staged)
    pub fn staged(&self) -> Option<BlockRef> {
        self.staged.and(self.view.held.tip())
    }

    /// Tip block above everything held: readable at once, dropped by [`reorg`](Self::reorg)
    pub fn apply(&mut self, changes: Changes) {
        let (name, height) = (self.name(), changes.tip().height);
        assert!(
            self.staged.is_none(),
            "{name}: apply {height} over staged blocks (finalize first)"
        );
        self.hold(changes, "blocks must arrive contiguously");
    }

    /// Final block above everything held, `weight` = its source bytes; `true` = a whole batch
    /// staged (finalize it)
    #[must_use = "a whole batch staged = finalize it"]
    pub fn stage(&mut self, changes: Changes, weight: usize) -> bool {
        let (name, height) = (self.name(), changes.tip().height);
        let applied = self.staged.is_none() && !self.view.held.blocks.is_empty();
        assert!(!applied, "{name}: final {height} above applied blocks");
        self.hold(changes, "final blocks must arrive contiguously");
        let staged = self.staged.unwrap_or(0).saturating_add(weight);
        self.staged = Some(staged);
        staged >= self.batch.get()
    }

    /// Held blocks through `through` → one commit (one fsync), then read from durable
    ///
    /// - staged blocks commit whole, never split
    /// - failed commit = panic naming the index and its directory (store poisoned: recovery =
    ///   restart)
    pub fn finalize(&mut self, through: Height) {
        let name = self.name();
        let durable = self.durable_tip().map(|tip| tip.height);
        let held = self.view.tip().map(|tip| tip.height);
        assert!(
            durable < Some(through) && Some(through) <= held,
            "{name}: finalize {through} outside (durable {durable:?}, held {held:?}]"
        );
        if let Some(staged) = self.staged() {
            assert_eq!(through, staged.height, "{name}: finalize {through} splits staged blocks");
        }

        let first = durable.map_or(Height::GENESIS, Height::next);
        let count = (u32::from(through) - u32::from(first)) as usize + 1;
        let changes = self.view.held.changes(count, self.store.schema());
        let committed = self.store.commit(changes);
        self.view.durable =
            committed.unwrap_or_else(|error| error.commit_failed(name, self.store.path()));
        self.view.held.release(count);
        if self.view.held.blocks.is_empty() {
            self.staged = None;
        }
    }

    /// Every applied block dropped (the producer replays the winning branch from durable)
    pub fn reorg(&mut self) {
        let name = self.name();
        assert!(self.staged.is_none(), "{name}: reorg with staged blocks (final never rolls back)");
        self.view.held = Held::empty(self.store.schema());
    }

    /// Store underneath (an engine's own checks)
    #[cfg(any(test, feature = "testing"))]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Held tiers = what their blocks added, contiguous above durable (after every conformance
    /// step)
    #[cfg(any(test, feature = "testing"))]
    pub fn check(&self, label: &str) {
        let staged_alone = self.staged.is_some() && self.view.held.blocks.is_empty();
        assert!(!staged_alone, "{label}: staged bytes, no block held");
        self.view.held.check(self.durable_tip(), label);
    }

    fn hold(&mut self, changes: Changes, contiguous: &str) {
        let name = self.name();
        assert_eq!(changes.schema(), self.store.schema(), "{name}: changes for another schema");
        let next = self.view.tip().map_or(Height::GENESIS, |tip| tip.height.next());
        let height = changes.tip().height;
        assert_eq!(height, next, "{name}: {contiguous} ({height} held next)");
        self.view.held.push(&changes);
    }

    fn name(&self) -> &'static str {
        self.store.schema().kind.name()
    }
}

impl Held {
    fn empty(schema: &Schema) -> Self {
        Self {
            blocks: Vector::new(),
            sequences: vec![Vector::new(); schema.sequences.len()],
            maps: vec![OrdMap::new(); schema.maps.len()],
        }
    }

    fn tip(&self) -> Option<BlockRef> {
        self.blocks.last().map(|block| block.tip)
    }

    fn sequence(&self, table: SequenceId) -> &Vector<Bytes> {
        let found = self.sequences.get(usize::from(table.0));
        found.unwrap_or_else(|| panic!("{table:?} not in the schema"))
    }

    fn map(&self, table: MapId) -> &OrdMap<Bytes, Bytes> {
        let found = self.maps.get(usize::from(table.0));
        found.unwrap_or_else(|| panic!("{table:?} not in the schema"))
    }

    /// `changes` as the newest block (a key held twice = a bug, as in the store)
    fn push(&mut self, changes: &Changes) {
        let schema = changes.schema();
        let appends = schema
            .sequence_ids()
            .zip(&mut self.sequences)
            .enumerate()
            .filter_map(|(at, (table, held))| {
                let before = held.len();
                held.extend(changes.appends(table).map(Bytes::copy_from_slice));
                (held.len() > before).then(|| (at, held.len() - before))
            })
            .collect();
        let keys = schema
            .map_ids()
            .zip(&mut self.maps)
            .map(|(table, held)| {
                let rows = changes.inserts(table).map(|(key, value)| {
                    let key = Bytes::copy_from_slice(key);
                    let twice = held.insert(key.clone(), Bytes::copy_from_slice(value));
                    assert!(twice.is_none(), "{}: a key held twice", schema.map(table).name);
                    key
                });
                rows.collect()
            })
            .collect();
        self.blocks.push_back(Arc::new(HeldBlock { tip: changes.tip(), appends, keys }));
    }

    /// Oldest `count` blocks as one commit, tipped by the last of them
    fn changes(&self, count: usize, schema: &Schema) -> Changes {
        let oldest = || self.blocks.iter().take(count);
        let tip = oldest().last().expect("finalize covers >= 1 held block").tip;
        let mut changes = Changes::new(tip, schema);
        let records = appended(oldest(), self.sequences.len());
        for ((table, held), records) in schema.sequence_ids().zip(&self.sequences).zip(records) {
            for record in held.iter().take(records) {
                changes.append(table, record);
            }
        }
        for (table, held) in schema.map_ids().zip(&self.maps) {
            for key in oldest().flat_map(|block| &block.keys[usize::from(table.0)]) {
                let value = held.get(key).expect("a held block's keys are held");
                changes.insert(table, key, value);
            }
        }
        changes
    }

    /// Oldest `count` blocks dropped (committed: durable answers for them)
    fn release(&mut self, count: usize) {
        let newer = self.blocks.split_off(count);
        let released = std::mem::replace(&mut self.blocks, newer);
        let records = appended(released.iter(), self.sequences.len());
        for (held, records) in self.sequences.iter_mut().zip(records) {
            *held = held.split_off(records);
        }
        for (at, held) in self.maps.iter_mut().enumerate() {
            for key in released.iter().flat_map(|block| &block.keys[at]) {
                held.remove(key);
            }
        }
    }

    #[cfg(any(test, feature = "testing"))]
    fn check(&self, durable: Option<BlockRef>, label: &str) {
        let mut next = durable.map_or(Height::GENESIS, |tip| tip.height.next());
        for block in &self.blocks {
            assert_eq!(block.tip.height, next, "{label}: held blocks contiguous above durable");
            next = next.next();
        }
        let records = appended(self.blocks.iter(), self.sequences.len());
        for (at, (held, records)) in self.sequences.iter().zip(records).enumerate() {
            assert_eq!(held.len(), records, "{label}: sequence {at} held = its blocks' appends");
        }
        for (at, held) in self.maps.iter().enumerate() {
            let keys: Vec<&Bytes> = self.blocks.iter().flat_map(|block| &block.keys[at]).collect();
            let all_held = keys.iter().all(|key| held.contains_key(*key));
            assert!(
                all_held && held.len() == keys.len(),
                "{label}: map {at} held = its blocks' keys"
            );
        }
    }
}

impl<V: View> TieredView<V> {
    /// What the store committed (the seam: at or below its tip = durable)
    pub fn durable(&self) -> &V {
        &self.durable
    }
}

impl<V: View> View for TieredView<V> {
    fn tip(&self) -> Option<BlockRef> {
        self.held.tip().or_else(|| self.durable.tip())
    }
}

impl<V: View> fmt::Debug for TieredView<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TieredView")
            .field("tip", &self.tip())
            .field("durable", &self.durable.tip())
            .finish_non_exhaustive()
    }
}

impl<V: SequenceRead> SequenceRead for TieredView<V> {
    fn len(&self, table: SequenceId) -> u64 {
        self.durable.len(table) + self.held.sequence(table).len() as u64
    }

    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes> {
        match at.checked_sub(self.durable.len(table)) {
            None => self.durable.record(table, at),
            Some(above) => self.held.sequence(table).get(above as usize).cloned(),
        }
    }

    fn records(&self, table: SequenceId, range: Range<u64>) -> Vec<Bytes> {
        let durable = self.durable.len(table);
        let below = range.start.min(durable)..range.end.min(durable);
        let mut records =
            if below.is_empty() { Vec::new() } else { self.durable.records(table, below) };
        let held = self.held.sequence(table);
        let above = range.start.max(durable) - durable..range.end.max(durable) - durable;
        records.extend(above.map(|at| {
            let record = held.get(at as usize).cloned();
            record.unwrap_or_else(|| panic!("{table:?}: records past len"))
        }));
        records
    }
}

impl<V: MapRead> MapRead for TieredView<V> {
    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes> {
        let held = self.held.map(table).get(key).cloned();
        held.or_else(|| self.durable.value(table, key))
    }

    fn values(&self, table: MapId, keys: &[&[u8]]) -> Vec<Option<Bytes>> {
        let held = self.held.map(table);
        let mut answers: Vec<Option<Bytes>> =
            keys.iter().map(|key| held.get(*key).cloned()).collect();
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
        let held = self.held.map(table).range::<_, [u8]>(bounds).take(left.saturating_add(1));
        let before = rows.len();
        rows.extend(held.map(|(key, value)| (key.clone(), value.clone())));
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
