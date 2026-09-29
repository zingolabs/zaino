//! Non-finalized rows (the fold applied, not yet on disk) and the [`ReadView`] over both tiers
//!
//! - same projection as the segments, one watermark earlier (`docs/design/non-finalized-state.md`)
//! - `imbl`: a view clones the rows in O(1) while `apply` keeps folding (shared structure)
//! - keyed exactly as the segments (a read merges the two with one row shape)

use std::sync::Arc;

use imbl::OrdMap;
use zaino_persistence::lsm::Snapshot;
use zaino_primitives::types::{Height, OutPoint, Zatoshis};

use crate::key::{AddressKey, ReceiveKey, ReceiveRow, Spend, SpentRow};

/// Non-finalized rows + both segment sets, taken together by the writer (one publication: a row
/// is in exactly one tier)
#[derive(Clone)]
pub struct ReadView {
    pub(crate) non_finalized: NonFinalizedRows,
    pub(crate) receives: Arc<Snapshot<ReceiveKey>>,
    pub(crate) spent: Arc<Snapshot<OutPoint>>,
}

impl std::fmt::Debug for ReadView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadView").field("applied", &self.non_finalized.applied).finish()
    }
}

impl ReadView {
    /// Receives of `address` from height `start` to the applied tip, both inclusive, ascending,
    /// non-finalized merged over the segments; `None` = more than `limit` (the walk stops there,
    /// never truncates)
    pub(crate) fn receives(
        &self,
        address: AddressKey,
        start: u32,
        limit: usize,
    ) -> Option<Vec<ReceiveRow>> {
        // key-range end, exclusive: the height after the applied tip
        let end = u32::from(self.non_finalized.applied.map_or(Height::GENESIS, Height::next));
        // nothing to walk (an inverted range panics `OrdMap`)
        if start >= end {
            return Some(Vec::new());
        }

        let mut rows: Vec<ReceiveRow> = self
            .non_finalized
            .receives_in(address, start, end)
            .take(limit.saturating_add(1))
            .collect();
        let left = limit.checked_sub(rows.len())?;
        rows.extend(self.receives.range_at_most::<ReceiveRow>(
            &ReceiveKey::first(address, start),
            &ReceiveKey::first(address, end),
            left,
        )?);
        rows.sort_unstable_by_key(|row| row.key);
        Some(rows)
    }

    /// Each of `addresses`' [`receives`](Self::receives) from height `start` (inclusive) that
    /// nothing has spent, in `addresses` order (one batched spend lookup across all of them);
    /// `limit` = rows across all of them
    pub(crate) fn unspent(
        &self,
        addresses: &[AddressKey],
        start: u32,
        limit: usize,
    ) -> Option<Vec<Vec<ReceiveRow>>> {
        let mut left = limit;
        let mut received: Vec<Vec<ReceiveRow>> = Vec::with_capacity(addresses.len());
        for &address in addresses {
            let rows = self.receives(address, start, left)?;
            left -= rows.len();
            received.push(rows);
        }
        let keys: Vec<ReceiveKey> = received.iter().flatten().map(|row| row.key).collect();
        let mut spends = self.spends_of(&keys).into_iter();
        Some(
            received
                .into_iter()
                .map(|rows| {
                    rows.into_iter()
                        .filter(|_| spends.next().expect("one spend lookup per receive").is_none())
                        .collect()
                })
                .collect(),
        )
    }

    /// What spent each of `received`, if the index has seen it spent, in `received` order
    ///
    /// - non-finalized first, the rest in one sorted `get_many` over the segments (every segment
    ///   but ≤ 1 answers each from its filter)
    pub(crate) fn spends_of(&self, received: &[ReceiveKey]) -> Vec<Option<Spend>> {
        let outpoints: Vec<OutPoint> =
            received.iter().map(|key| OutPoint { txid: key.txid, vout: key.vout }).collect();
        let mut spends: Vec<Option<Spend>> = outpoints
            .iter()
            .map(|outpoint| self.non_finalized.spent.get(outpoint).copied())
            .collect();
        let unheld: Vec<usize> = (0..spends.len()).filter(|&at| spends[at].is_none()).collect();
        let keys: Vec<OutPoint> = unheld.iter().map(|&at| outpoints[at]).collect();
        for (at, row) in unheld.into_iter().zip(self.spent.get_many::<SpentRow>(&keys)) {
            spends[at] = row.map(|row| row.spend);
        }
        spends
    }
}

/// Rows applied above the durable tip (dropped wholesale on a reorg)
///
/// - `applied` = last folded height, inclusive (`None` = nothing held)
#[derive(Clone, Default)]
pub(crate) struct NonFinalizedRows {
    receives: OrdMap<ReceiveKey, Zatoshis>,
    spent: OrdMap<OutPoint, Spend>,
    applied: Option<Height>,
}

impl NonFinalizedRows {
    /// Nothing buffered above `applied` (last held height, inclusive; `None` = empty index): open,
    /// and every drain back to empty
    pub(crate) fn empty_at(applied: Option<Height>) -> Self {
        Self { receives: OrdMap::new(), spent: OrdMap::new(), applied }
    }

    /// Last folded height, inclusive (what serving answers up to; `None` = nothing held)
    pub(crate) fn applied(&self) -> Option<Height> {
        self.applied
    }

    pub(crate) fn insert_receive(&mut self, row: ReceiveRow) {
        self.receives.insert(row.key, row.value);
    }

    pub(crate) fn insert_spend(&mut self, row: SpentRow) {
        self.spent.insert(row.key, row.spend);
    }

    /// `height` folded
    pub(crate) fn advance(&mut self, height: Height) {
        self.applied = Some(height);
    }

    /// Everything at or below `tip` (last height, inclusive; `None` = nothing), ascending (what a
    /// commit makes durable), kept until it lands
    pub(crate) fn rows_through(&self, tip: Option<Height>) -> (Vec<ReceiveRow>, Vec<SpentRow>) {
        let within = |height: u32| tip.is_some_and(|tip| height <= u32::from(tip));
        let receives: Vec<_> = self
            .receives
            .iter()
            .filter(|(key, _)| within(key.height))
            .map(|(key, value)| ReceiveRow { key: *key, value: *value })
            .collect();
        let spent: Vec<_> = self
            .spent
            .iter()
            .filter(|(_, spend)| within(spend.height))
            .map(|(key, spend)| SpentRow { key: *key, spend: *spend })
            .collect();
        (receives, spent)
    }

    /// Drops everything at or below `tip` (last height, inclusive; landed: durable segments answer
    /// for it now)
    ///
    /// - lifts `applied` to `tip` (bulk-sync blocks finalize without ever being applied)
    pub(crate) fn land_through(&mut self, tip: Option<Height>) {
        let (receives, spent) = self.rows_through(tip);
        for row in &receives {
            self.receives.remove(&row.key);
        }
        for row in &spent {
            self.spent.remove(&row.key);
        }
        self.applied = self.applied.max(tip);
    }

    /// Buffered receives of `address` from height `start` inclusive to `end` exclusive, ascending
    pub(crate) fn receives_in(
        &self,
        address: AddressKey,
        start: u32,
        end: u32,
    ) -> impl Iterator<Item = ReceiveRow> + '_ {
        self.receives
            .range(ReceiveKey::first(address, start)..ReceiveKey::first(address, end))
            .map(|(key, value)| ReceiveRow { key: *key, value: *value })
    }
}
