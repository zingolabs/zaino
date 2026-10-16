//! Address RPCs over one [`TransparentAddressReader`] (one per request, as of the served tip)
//!
//! - scan `receives`, then one batched `spent` lookup over every outpoint (unspent = a miss)
//! - synchronous: mmapped pages and range walks, so a transport runs these off its async workers

use std::num::NonZeroUsize;

use zaino_persistence::MapRead;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;
use zcash_transparent::address::TransparentAddress;
use zcash_transparent::bundle::OutPoint;

use crate::{key::AddressKey, TransparentAddressReader};

/// - never an empty result (gap-limit walk must tell "no transactions" from "cannot tell")
/// - transport maps these onto gRPC codes (no transport named in this crate)
/// - `SupplyExceeded` = corrupt store
/// - `TooManyRows` = an exchange's addresses, refused whole (never truncated: a short answer reads
///   as a balance)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("unspent total for this address exceeds the money supply")]
    SupplyExceeded,

    #[error("these addresses have more than {limit} receives, the per-request limit")]
    TooManyRows { limit: usize },
}

/// Receives one request may walk unless an operator overrides it
///
/// - light wallet's addresses hold tens to thousands
/// - ~10 ms of range walk at the limit
pub const DEFAULT_MAX_ADDRESS_ROWS: NonZeroUsize =
    NonZeroUsize::new(100_000).expect("100000 is non-zero");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressUtxo {
    pub outpoint: OutPoint,
    pub height: BlockHeight,
    pub value: Zatoshis,
}

/// Transaction paying the queried address or spending from it
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransactionRef {
    pub(crate) height: BlockHeight,
    pub txid: TxId,
}

impl<V: MapRead> TransparentAddressReader<V> {
    fn too_many(&self) -> ServeError {
        ServeError::TooManyRows { limit: self.max_rows().get() }
    }

    /// `GetAddressUtxos`: unspent receives from height `start` (inclusive) to the tip, oldest
    /// first, per address in `addresses` order (one batched spend lookup across all of them)
    pub fn utxos_of(
        &self,
        addresses: &[TransparentAddress],
        start: BlockHeight,
    ) -> Result<Vec<Vec<AddressUtxo>>, ServeError> {
        let keys: Vec<AddressKey> = addresses.iter().map(AddressKey::from).collect();
        let unspent = self
            .unspent(&keys, u32::from(start), self.max_rows().get())
            .ok_or_else(|| self.too_many())?;
        Ok(unspent
            .into_iter()
            .map(|rows| {
                rows.into_iter()
                    .map(|row| AddressUtxo {
                        outpoint: OutPoint::new(row.key.txid.into(), row.key.vout),
                        height: BlockHeight::from(row.key.height),
                        value: row.value,
                    })
                    .collect()
            })
            .collect())
    }

    /// `GetTaddressBalance`: sum of everything unspent, all of history, per address in
    /// `addresses` order (one batched spend lookup across all of them)
    pub fn balances(&self, addresses: &[TransparentAddress]) -> Result<Vec<Zatoshis>, ServeError> {
        let keys: Vec<AddressKey> = addresses.iter().map(AddressKey::from).collect();
        self.balances_of(&keys)
    }

    /// [`balances`](Self::balances) by storage key (reaches the opaque key no address parses to)
    pub(crate) fn balances_of(&self, keys: &[AddressKey]) -> Result<Vec<Zatoshis>, ServeError> {
        self.unspent(keys, 0, self.max_rows().get())
            .ok_or_else(|| self.too_many())?
            .into_iter()
            .map(|rows| {
                rows.iter()
                    .map(|row| row.value)
                    .sum::<Option<Zatoshis>>()
                    .ok_or(ServeError::SupplyExceeded)
            })
            .collect()
    }

    /// `GetTaddressTransactions`: txs from height `start` to `end`, both inclusive, paying the
    /// address or spending from it
    ///
    /// - scanned over all of history, not the range (an in-range spend consumes an output received
    ///   at any earlier height, outpoint = all the block carried)
    pub fn transactions(
        &self,
        address: &TransparentAddress,
        start: BlockHeight,
        end: BlockHeight,
    ) -> Result<Vec<TransactionRef>, ServeError> {
        assert!(start <= end, "range {start}..={end} reversed (ordered at the router)");
        let (start, end) = (u32::from(start), u32::from(end));

        let received = self
            .receives(address.into(), 0, self.max_rows().get())
            .ok_or_else(|| self.too_many())?;
        let keys: Vec<_> = received.iter().map(|row| row.key).collect();
        let mut found = Vec::new();
        for (row, spend) in received.iter().zip(self.spends_of(&keys)) {
            if (start..=end).contains(&row.key.height) {
                let height = BlockHeight::from(row.key.height);
                found.push(TransactionRef { height, txid: row.key.txid });
            }

            let Some(spend) = spend else {
                continue;
            };
            if (start..=end).contains(&spend.height) {
                let height = BlockHeight::from(spend.height);
                found.push(TransactionRef { height, txid: spend.spender });
            }
        }

        // one tx can pay an address and spend from it, either one twice
        found.sort_unstable();
        found.dedup();

        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use zaino_persistence::{fs::SimFs, Schema, Store};
    use zaino_source::testing::{h, p2pkh, MockChain};
    use zaino_sync::testing::open;
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::fold;

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    /// - 0, 1 durable, 2 buffered (durable view + layer): 1 pays `paid` 42, 2 spends it
    /// - Tip 2: spend counts
    /// - `as_of(1)` hides it (view ahead of the served tip)
    /// - Stranger + heights past the tip → empty, never an error
    #[test]
    fn as_of_hides_rows_past_the_served_tip_and_an_unpaid_address_answers_empty() {
        let (paid, stranger) = ([0x01; 20], [0xff; 20]);
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| b.coinbase(|c| c.pay(&p2pkh(paid), 42)));
        let paid_out = chain.output(one, 0, 0);
        let two = chain.mine(|b| b.tx(|t| t.spend(paid_out.clone()).pay(&p2pkh([0x02; 20]), 41)));
        let paid = TransparentAddress::PublicKeyHash(paid);
        let stranger = TransparentAddress::ScriptHash(stranger);
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        for (height, block) in chain.blocks(two).iter().enumerate() {
            let mut changes = store.changes(block.block_ref());
            fold(&TransparentAddressReader::new(store.applied()), block, &mut changes);
            store.apply(changes);
            if height == 1 {
                store.commit().expect("SimFs commit");
            }
        }
        let at_two = TransparentAddressReader::new(store.applied());
        let at_one = at_two.clone().as_of(h(1));
        let received = AddressUtxo {
            outpoint: paid_out,
            height: h(1),
            value: Zatoshis::from_u64(42).expect("in supply"),
        };
        let paying = TransactionRef { height: h(1), txid: chain.tx(one, 0).txid };
        let spending = TransactionRef { height: h(2), txid: chain.tx(two, 1).txid };
        let receiver = TransparentAddress::PublicKeyHash([0x02; 20]);

        let balances = at_two.balances(&[paid, receiver]);
        assert_eq!(balances, Ok(vec![Zatoshis::ZERO, Zatoshis::from_u64(41).expect("in supply")]));
        assert_eq!(at_two.utxos_of(&[paid], h(0)), Ok(vec![Vec::new()]), "spent at 2");
        assert_eq!(at_two.transactions(&paid, h(0), h(2)), Ok(vec![paying, spending]));
        let balances = at_one.balances(&[paid, receiver]);
        let past_the_tip = Ok(vec![received.value, Zatoshis::ZERO]);
        assert_eq!(balances, past_the_tip, "2's spend and receive past the tip");
        assert_eq!(at_one.utxos_of(&[paid], h(0)), Ok(vec![vec![received]]));
        assert_eq!(at_one.transactions(&paid, h(0), h(2)), Ok(vec![paying]));

        for reader in [&at_two, &at_one] {
            assert_eq!(reader.utxos_of(&[stranger], h(0)), Ok(vec![Vec::new()]));
            assert_eq!(reader.transactions(&stranger, h(0), h(2)), Ok(Vec::new()));
            assert_eq!(reader.balances(&[stranger]), Ok(vec![Zatoshis::ZERO]));
            assert_eq!(reader.transactions(&paid, h(3), h(3)), Ok(Vec::new()), "past the tip");
            assert_eq!(reader.utxos_of(&[paid], h(3)), Ok(vec![Vec::new()]), "past the tip");
        }
    }

    /// - Row budget = receives across every address of a request, durable + buffered alike
    /// - At the limit every method answers
    /// - One over → every method refuses whole (never a short list or a partial balance)
    #[test]
    fn a_request_over_its_row_budget_is_refused_whole_by_every_method() {
        let (first, second) = (
            TransparentAddress::PublicKeyHash([0x01; 20]),
            TransparentAddress::PublicKeyHash([0x02; 20]),
        );
        // - `first` paid at 1, 2 and 3, `second` at 2
        // - 0..=2 durable, 3 buffered
        let (pays_first, pays_second) = (p2pkh([0x01; 20]), p2pkh([0x02; 20]));
        let mut chain = MockChain::regtest();
        chain.mine(|b| b.coinbase(|c| c.pay(&pays_first, 10)));
        chain.mine(|b| b.coinbase(|c| c.pay(&pays_first, 10).pay(&pays_second, 10)));
        let three = chain.mine(|b| b.coinbase(|c| c.pay(&pays_first, 10)));
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        for (height, block) in chain.blocks(three).iter().enumerate() {
            let mut changes = store.changes(block.block_ref());
            fold(&TransparentAddressReader::new(store.applied()), block, &mut changes);
            store.apply(changes);
            if height == 2 {
                store.commit().expect("SimFs commit");
            }
        }
        let budget = |rows: usize| {
            TransparentAddressReader::new(store.applied())
                .with_max_rows(NonZeroUsize::new(rows).expect("non-zero"))
        };
        let over = |rows: usize| Some(ServeError::TooManyRows { limit: rows });
        let both = [first, second];

        let zats = |zats: u64| Zatoshis::from_u64(zats).expect("in supply");
        let rows = |utxos: Result<Vec<Vec<AddressUtxo>>, _>| utxos.map(|all| all[0].len());

        // `first` alone = 3 rows (2 durable + 1 buffered), both = 4
        let at = budget(3);
        assert_eq!(at.balances(&[first]), Ok(vec![zats(30)]));
        assert_eq!(rows(at.utxos_of(&[first], h(0))), Ok(3));
        assert_eq!(at.transactions(&first, h(0), h(3)).map(|found| found.len()), Ok(3));
        assert_eq!(at.balances(&both).err(), over(3), "3 + 1 across the request's addresses");
        assert_eq!(at.utxos_of(&both, h(0)).err(), over(3));

        let under = budget(2);
        assert_eq!(under.balances(&[first]).err(), over(2));
        assert_eq!(under.utxos_of(&[first], h(0)).err(), over(2));
        let narrow = under.transactions(&first, h(3), h(3)).err();
        assert_eq!(narrow, over(2), "a narrow range still walks the whole history");
        assert_eq!(rows(under.utxos_of(&[first], h(2))), Ok(2), "from 2: 2 rows");

        assert_eq!(budget(4).balances(&both), Ok(vec![zats(30), zats(10)]));
    }
}
