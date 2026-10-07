//! [`ReadView`]: both maps over every tier, pinned once per request
//!
//! - held blocks keyed exactly as the maps (`zaino_persistence::TieredView` merges the two)

use zaino_persistence::{MapRead, TieredView, View};
use zaino_primitives::types::{Height, OutPoint};

use crate::{
    key::{
        decode_receive, decode_spend, encode_receive_key, AddressKey, ReceiveKey, ReceiveRow, Spend,
    },
    RECEIVES, SPENT,
};

/// Held blocks + the committed store (one publication: a row in exactly one tier)
#[derive(Clone)]
pub struct ReadView<V> {
    view: TieredView<V>,
}

impl<V: View> std::fmt::Debug for ReadView<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadView").field("view", &self.view).finish()
    }
}

impl<V: MapRead> ReadView<V> {
    pub(crate) fn new(view: TieredView<V>) -> Self {
        Self { view }
    }

    /// Receives of `address` from height `start` to the tip, both inclusive, ascending; `None` =
    /// more than `limit` (the walk stops there, never truncates)
    pub(crate) fn receives(
        &self,
        address: AddressKey,
        start: u32,
        limit: usize,
    ) -> Option<Vec<ReceiveRow>> {
        // key-range end, exclusive: the height after the tip
        let end = u32::from(self.view.tip().map_or(Height::GENESIS, |tip| tip.height.next()));
        // start past the tip: nothing held
        if start >= end {
            return Some(Vec::new());
        }
        let (from, to) = (ReceiveKey::first(address, start), ReceiveKey::first(address, end));
        let (from, to) = (encode_receive_key(&from), encode_receive_key(&to));
        let rows = self.view.range(RECEIVES, &from, &to, limit)?;
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

    /// What spent each of `received`, if the index has seen it spent, in `received` order (one
    /// `values(SPENT, ..)` batch)
    pub(crate) fn spends_of(&self, received: &[ReceiveKey]) -> Vec<Option<Spend>> {
        let keys: Vec<[u8; OutPoint::LEN]> = received
            .iter()
            .map(|key| OutPoint { txid: key.txid, vout: key.vout }.encode())
            .collect();
        let keys: Vec<&[u8]> = keys.iter().map(|key| &key[..]).collect();
        let values = self.view.values(SPENT, &keys).into_iter();
        let spend =
            |value: &[u8]| decode_spend(value.try_into().expect("spent: SPEND-wide values"));
        values.map(|value| value.as_deref().map(spend)).collect()
    }
}
