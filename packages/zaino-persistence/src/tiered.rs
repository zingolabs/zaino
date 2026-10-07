//! Blocks above a store's committed tip, read over its durable view (`persistence-engine.md` §5)
//!
//! - one block = one [`Changes`] (its tip = that block), held keyed exactly as the store holds it
//! - staged = final blocks in the store's buffer ([`Store::apply`]), batched by source bytes
//! - applied = reorgable tip blocks in a [`Layer`] over durable, applied to the store on finalize
//! - never both at once (a tip block builds on durable: staged blocks commit first)

use std::{collections::VecDeque, num::NonZeroUsize};

use zaino_primitives::types::{BlockRef, Height};

use crate::{
    layer::{Layer, LayeredView},
    port::{Changes, Schema, Store, View},
};

/// One index's store + the blocks held above its committed tip (one writer)
///
/// - `weight` = source bytes staged; `layer` = `applied` folded over durable
pub struct Tiered<S: Store> {
    store: S,
    batch: NonZeroUsize,
    weight: usize,
    applied: VecDeque<Changes>,
    layer: Layer,
}

impl<S: Store> Tiered<S> {
    /// Nothing held above `store`'s committed tip; `batch` = source bytes per staged commit
    pub fn new(store: S, batch: NonZeroUsize) -> Self {
        let layer = Layer::empty(store.schema());
        Self { store, batch, weight: 0, applied: VecDeque::new(), layer }
    }

    pub fn schema(&self) -> &Schema {
        self.store.schema()
    }

    /// Everything held over everything committed (a clone per publication)
    pub fn view(&self) -> LayeredView<S::View> {
        match self.applied.is_empty() {
            true => self.store.staged(),
            false => LayeredView::new(self.store.view(), self.layer.clone()),
        }
    }

    /// Last committed block (`None` = nothing committed)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.store.view().tip()
    }

    /// Last applied block, else the durable tip (staged = final, never applied)
    pub fn applied(&self) -> Option<BlockRef> {
        self.layer.tip().or_else(|| self.durable_tip())
    }

    /// Last staged block (`None` = nothing staged)
    pub fn staged(&self) -> Option<BlockRef> {
        self.store.staged().tip().filter(|&tip| Some(tip) != self.durable_tip())
    }

    /// Tip block above everything held: readable at once, dropped by [`reorg`](Self::reorg)
    pub fn apply(&mut self, changes: Changes) {
        let (name, height) = (self.name(), changes.tip().height);
        assert!(
            self.staged().is_none(),
            "{name}: apply {height} over staged blocks (finalize first)"
        );
        self.assert_next(&changes, "blocks must arrive contiguously");
        self.layer = self.layer.with(&changes);
        self.applied.push_back(changes);
    }

    /// Final block above everything held, `weight` = its source bytes; `true` = a whole batch
    /// staged (finalize it)
    #[must_use = "a whole batch staged = finalize it"]
    pub fn stage(&mut self, changes: Changes, weight: usize) -> bool {
        let (name, height) = (self.name(), changes.tip().height);
        assert!(self.applied.is_empty(), "{name}: final {height} above applied blocks");
        self.assert_next(&changes, "final blocks must arrive contiguously");
        self.store.apply(changes);
        self.weight = self.weight.saturating_add(weight);
        self.weight >= self.batch.get()
    }

    /// Held blocks through `through` → one commit (one fsync), then read from durable
    ///
    /// - staged blocks commit whole, never split
    /// - failed commit = panic naming the index and its directory (store poisoned: recovery =
    ///   restart)
    pub fn finalize(&mut self, through: Height) {
        let name = self.name();
        let durable = self.durable_tip().map(|tip| tip.height);
        let held = self.view().tip().map(|tip| tip.height);
        assert!(
            durable < Some(through) && Some(through) <= held,
            "{name}: finalize {through} outside (durable {durable:?}, held {held:?}]"
        );
        if let Some(staged) = self.staged() {
            assert_eq!(through, staged.height, "{name}: finalize {through} splits staged blocks");
        }

        let count =
            self.applied.iter().take_while(|changes| changes.tip().height <= through).count();
        for changes in self.applied.drain(..count) {
            self.store.apply(changes);
        }
        let committed = self.store.commit();
        committed.unwrap_or_else(|error| error.commit_failed(name, self.store.path()));
        self.weight = 0;
        self.layer = self.layer.rebase(&self.store.view());
    }

    /// Every applied block dropped (the producer replays the winning branch from durable)
    pub fn reorg(&mut self) {
        let name = self.name();
        assert!(
            self.staged().is_none(),
            "{name}: reorg with staged blocks (final never rolls back)"
        );
        self.applied.clear();
        self.layer = Layer::empty(self.store.schema());
    }

    /// Next height above everything held, for this store's schema
    fn assert_next(&self, changes: &Changes, contiguous: &str) {
        let name = self.name();
        assert_eq!(changes.schema(), self.store.schema(), "{name}: changes for another schema");
        let next = self.view().tip().map_or(Height::GENESIS, |tip| tip.height.next());
        let height = changes.tip().height;
        assert_eq!(height, next, "{name}: {contiguous} ({height} held next)");
    }

    fn name(&self) -> &'static str {
        self.store.schema().kind.name()
    }

    /// Applied blocks contiguous above durable, `layer` = them folded; staged xor applied
    #[cfg(test)]
    fn check(&self, label: &str) {
        let mixed = !self.applied.is_empty() && (self.staged().is_some() || self.weight > 0);
        assert!(!mixed, "{label}: staged and applied blocks at once");
        let mut next = self.durable_tip().map_or(Height::GENESIS, |tip| tip.height.next());
        let mut folded = Layer::empty(self.store.schema());
        for changes in &self.applied {
            assert_eq!(changes.tip().height, next, "{label}: applied contiguous above durable");
            next = next.next();
            folded = folded.with(changes);
        }
        assert!(self.layer == folded, "{label}: layer = the applied blocks folded");
        self.layer.check(label);
    }
}

#[cfg(test)]
mod tests;
