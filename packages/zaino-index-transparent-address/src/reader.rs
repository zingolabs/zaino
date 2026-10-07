//! [`TransparentAddressReader`]: typed reads of both maps over any view of them

use std::num::NonZeroUsize;

use zaino_persistence::{MapRead, View};
use zaino_primitives::types::{Height, OutPoint};

use crate::{
    key::{
        decode_receive, decode_spend, encode_receive_key, AddressKey, ReceiveKey, ReceiveRow, Spend,
    },
    DEFAULT_MAX_ADDRESS_ROWS, RECEIVES, SPENT,
};

/// One state of both maps, read as of `tip` (a snapshot's served tip: rows above it unseen)
///
/// - `max_rows` = receives one request may walk, across all its addresses
#[derive(Clone)]
pub struct TransparentAddressReader<V> {
    view: V,
    tip: Option<Height>,
    max_rows: NonZeroUsize,
}

impl<V: View> std::fmt::Debug for TransparentAddressReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tip = self.view.tip();
        f.debug_struct("TransparentAddressReader").field("tip", &tip).finish_non_exhaustive()
    }
}

impl<V: View> TransparentAddressReader<V> {
    /// `view` of a store opened with [`TABLES`](crate::TABLES), as of its own tip
    pub fn new(view: V) -> Self {
        let tip = view.tip().map(|tip| tip.height);
        Self { view, tip, max_rows: DEFAULT_MAX_ADDRESS_ROWS }
    }

    /// Rows above `tip` unseen (a view ahead of the snapshot it serves)
    pub fn as_of(mut self, tip: Height) -> Self {
        self.tip = self.tip.min(Some(tip));
        self
    }

    /// Overrides [`DEFAULT_MAX_ADDRESS_ROWS`]
    pub fn with_max_rows(mut self, max_rows: NonZeroUsize) -> Self {
        self.max_rows = max_rows;
        self
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }

    pub(crate) fn max_rows(&self) -> NonZeroUsize {
        self.max_rows
    }
}

impl<V: MapRead> TransparentAddressReader<V> {
    /// Receives of `address` from height `start` to the tip, both inclusive, ascending; `None` =
    /// more than `limit` (the walk stops there, never truncates)
    pub(crate) fn receives(
        &self,
        address: AddressKey,
        start: u32,
        limit: usize,
    ) -> Option<Vec<ReceiveRow>> {
        // key-range end, exclusive: the height after the tip
        let end = u32::from(self.tip.map_or(Height::GENESIS, Height::next));
        // start past the tip: nothing held
        if start >= end {
            return Some(Vec::new());
        }
        let (from, to) = (ReceiveKey::first(address, start), ReceiveKey::first(address, end));
        let (from, to) = (encode_receive_key(&from), encode_receive_key(&to));
        let rows = self.view.map(RECEIVES).range(&from, &to, limit)?;
        let rows = rows.iter().map(|(key, value)| {
            let key = key[..].try_into().expect("receives: RECEIVE_KEY-wide keys");
            decode_receive(key, value[..].try_into().expect("receives: RECEIVE_VALUE-wide values"))
        });
        Some(rows.collect())
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

    /// What spent each of `received` at or below the tip, in `received` order (one
    /// `map(SPENT).values(..)` batch)
    pub(crate) fn spends_of(&self, received: &[ReceiveKey]) -> Vec<Option<Spend>> {
        let keys: Vec<[u8; OutPoint::LEN]> = received
            .iter()
            .map(|key| OutPoint { txid: key.txid, vout: key.vout }.encode())
            .collect();
        let keys: Vec<&[u8]> = keys.iter().map(|key| &key[..]).collect();
        let values = self.view.map(SPENT).values(&keys).into_iter();
        let tip = self.tip.map(u32::from);
        let spend = |value: &[u8]| {
            let spend = decode_spend(value.try_into().expect("spent: SPEND-wide values"));
            (Some(spend.height) <= tip).then_some(spend)
        };
        values.map(|value| value.as_deref().and_then(spend)).collect()
    }
}
