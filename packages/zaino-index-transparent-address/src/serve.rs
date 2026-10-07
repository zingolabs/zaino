//! Address RPCs over one [`TransparentAddressReader`] (one per request, as of the served tip)
//!
//! - scan `receives`, then one batched `spent` lookup over every outpoint (unspent = a miss)
//! - synchronous: mmapped pages and range walks, so a transport runs these off its async workers

use std::num::NonZeroUsize;

use zaino_persistence::MapRead;
use zaino_primitives::types::{Height, OutPoint, TransactionId, Zatoshis};
use zcash_transparent::address::TransparentAddress;

use crate::{key::AddressKey, TransparentAddressReader};

/// - never an empty result (gap-limit walk must tell "no transactions" from "cannot tell")
/// - transport maps these onto gRPC codes; this crate names no transport
/// - `SupplyExceeded` = corrupt store; `TooManyRows` = an exchange's addresses, refused whole
///   (never truncated: a short answer reads as a balance)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("unspent total for this address exceeds the money supply")]
    SupplyExceeded,

    #[error("these addresses have more than {limit} receives, the per-request limit")]
    TooManyRows { limit: usize },
}

/// Receives one request may walk unless an operator overrides it
///
/// - a light wallet's addresses hold tens to thousands; ~10 ms of range walk at the limit
pub const DEFAULT_MAX_ADDRESS_ROWS: NonZeroUsize =
    NonZeroUsize::new(100_000).expect("100000 is non-zero");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressUtxo {
    pub outpoint: OutPoint,
    pub height: Height,
    pub value: Zatoshis,
}

/// Transaction paying the queried address or spending from it
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransactionRef {
    pub(crate) height: Height,
    pub txid: TransactionId,
}

/// Stored height → [`Height`] (rows = sealed, checksummed `fold` output, written from a `Height`)
fn stored(height: u32) -> Height {
    Height::try_from(height).expect("transparent-address rows: height written from a Height")
}

impl<V: MapRead> TransparentAddressReader<V> {
    fn too_many(&self) -> ServeError {
        ServeError::TooManyRows { limit: self.max_rows().get() }
    }

    #[cfg(test)]
    pub(crate) fn utxos(
        &self,
        address: &TransparentAddress,
        start: Height,
    ) -> Result<Vec<AddressUtxo>, ServeError> {
        let mut utxos = self.utxos_of(std::slice::from_ref(address), start)?;
        Ok(utxos.pop().expect("one list per address"))
    }

    /// `GetAddressUtxos`: unspent receives from height `start` (inclusive) to the tip, oldest
    /// first, per address in `addresses` order (one batched spend lookup across all of them)
    pub fn utxos_of(
        &self,
        addresses: &[TransparentAddress],
        start: Height,
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
                        outpoint: OutPoint { txid: row.key.txid, vout: row.key.vout },
                        height: stored(row.key.height),
                        value: row.value,
                    })
                    .collect()
            })
            .collect())
    }

    #[cfg(test)]
    pub(crate) fn balance(&self, address: &TransparentAddress) -> Result<Zatoshis, ServeError> {
        self.balance_of(address.into())
    }

    /// `GetTaddressBalance`: sum of everything unspent, all of history, per address in
    /// `addresses` order (one batched spend lookup across all of them)
    pub fn balances(&self, addresses: &[TransparentAddress]) -> Result<Vec<Zatoshis>, ServeError> {
        let keys: Vec<AddressKey> = addresses.iter().map(AddressKey::from).collect();
        self.balances_of(&keys)
    }

    /// One balance by storage key (reaches the opaque key no address parses to)
    #[cfg(test)]
    pub(crate) fn balance_of(&self, key: AddressKey) -> Result<Zatoshis, ServeError> {
        Ok(self.balances_of(&[key])?.pop().expect("one balance per key"))
    }

    fn balances_of(&self, keys: &[AddressKey]) -> Result<Vec<Zatoshis>, ServeError> {
        self.unspent(keys, 0, self.max_rows().get())
            .ok_or_else(|| self.too_many())?
            .into_iter()
            .map(|rows| {
                Zatoshis::sum_balances(rows.iter().map(|row| row.value))
                    .ok_or(ServeError::SupplyExceeded)
            })
            .collect()
    }

    /// `GetTaddressTransactions`: txs from height `start` to `end`, both inclusive, paying the
    /// address or spending from it
    ///
    /// - scanned over all of history, not the range (an in-range spend consumes an output received
    ///   at any earlier height; the outpoint = all the block carried)
    pub fn transactions(
        &self,
        address: &TransparentAddress,
        start: Height,
        end: Height,
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
                found.push(TransactionRef { height: stored(row.key.height), txid: row.key.txid });
            }

            let Some(spend) = spend else {
                continue;
            };
            if (start..=end).contains(&spend.height) {
                found.push(TransactionRef { height: stored(spend.height), txid: spend.spender });
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
    use std::path::Path;

    use zaino_persistence::{fs::SimFs, DiskEngine, IndexKind, PersistenceEngine, Schema, Store};
    use zaino_primitives::testing::{h, outpoint, p2pkh, MockChain};
    use zaino_primitives::types::OutPoint;
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{fold, FORMAT, TABLES};

    const SCHEMA: Schema =
        Schema::new(IndexKind::TransparentAddress, FORMAT, NetworkType::Regtest, TABLES);

    /// - 0, 1 committed, 2 buffered (committed view + layer): 1 pays `paid` 42, 2 spends it
    /// - Tip 2: spend counts; `as_of(1)` hides it (view ahead of the served tip)
    /// - Stranger + heights past the tip → empty, never an error
    #[test]
    fn as_of_hides_rows_past_the_served_tip_and_an_unpaid_address_answers_empty() {
        let (paid, stranger) = ([0x01; 20], [0xff; 20]);
        let mut chain = MockChain::regtest();
        chain.mine(|b| b.coinbase(|c| c.txid([0x77; 32]).pay(&p2pkh(paid), 42)));
        let two = chain.mine(|b| {
            b.tx(|t| t.txid([0x78; 32]).spend(outpoint([0x77; 32], 0)).pay(&p2pkh([0x02; 20]), 41))
        });
        let paid = TransparentAddress::PublicKeyHash(paid);
        let stranger = TransparentAddress::ScriptHash(stranger);
        let mut store =
            DiskEngine::new(SimFs::new()).open(Path::new("/ta"), &SCHEMA).expect("open");
        for (height, block) in chain.blocks(two).iter().enumerate() {
            let mut changes = store.changes(block.at());
            fold(&TransparentAddressReader::new(store.staged()), block, &mut changes);
            store.apply(changes);
            if height == 1 {
                store.commit().expect("SimFs commit");
            }
        }
        let at_two = TransparentAddressReader::new(store.staged());
        let at_one = at_two.clone().as_of(h(1));
        let received = AddressUtxo {
            outpoint: OutPoint { txid: TransactionId::from([0x77; 32]), vout: 0 },
            height: h(1),
            value: Zatoshis::new(42).expect("in supply"),
        };
        let paying = TransactionRef { height: h(1), txid: TransactionId::from([0x77; 32]) };
        let spending = TransactionRef { height: h(2), txid: TransactionId::from([0x78; 32]) };

        assert_eq!(at_two.balance(&paid), Ok(Zatoshis::ZERO), "spent at 2");
        assert_eq!(at_two.utxos(&paid, h(0)), Ok(Vec::new()));
        assert_eq!(at_two.transactions(&paid, h(0), h(2)), Ok(vec![paying, spending]));
        let at_one_balance = at_one.balance(&paid).map(Zatoshis::as_u64);
        assert_eq!(at_one_balance, Ok(42), "2's spend past the tip");
        assert_eq!(at_one.utxos(&paid, h(0)), Ok(vec![received]));
        assert_eq!(at_one.transactions(&paid, h(0), h(2)), Ok(vec![paying]));
        let receiver = TransparentAddress::PublicKeyHash([0x02; 20]);
        assert_eq!(at_one.balance(&receiver), Ok(Zatoshis::ZERO), "2's receive past the tip");

        for reader in [&at_two, &at_one] {
            assert_eq!(reader.utxos(&stranger, h(0)), Ok(Vec::new()));
            assert_eq!(reader.transactions(&stranger, h(0), h(2)), Ok(Vec::new()));
            assert_eq!(reader.balance(&stranger), Ok(Zatoshis::ZERO));
            assert_eq!(reader.transactions(&paid, h(3), h(3)), Ok(Vec::new()), "past the tip");
            assert_eq!(reader.utxos(&paid, h(3)), Ok(Vec::new()), "past the tip");
        }
    }

    /// - Row budget = receives across every address of a request, committed + buffered alike
    /// - At the limit every method answers; one over, every method refuses whole (never a short
    ///   list or a partial balance)
    #[test]
    fn a_request_over_its_row_budget_is_refused_whole_by_every_method() {
        let (first, second) = (
            TransparentAddress::PublicKeyHash([0x01; 20]),
            TransparentAddress::PublicKeyHash([0x02; 20]),
        );
        // `first` paid at 1, 2 and 3, `second` at 2; 0..=2 committed, 3 buffered
        let (pays_first, pays_second) = (p2pkh([0x01; 20]), p2pkh([0x02; 20]));
        let mut chain = MockChain::regtest();
        chain.mine(|b| b.coinbase(|c| c.pay(&pays_first, 10)));
        chain.mine(|b| b.coinbase(|c| c.pay(&pays_first, 10).pay(&pays_second, 10)));
        let three = chain.mine(|b| b.coinbase(|c| c.pay(&pays_first, 10)));
        let mut store =
            DiskEngine::new(SimFs::new()).open(Path::new("/ta"), &SCHEMA).expect("open");
        for (height, block) in chain.blocks(three).iter().enumerate() {
            let mut changes = store.changes(block.at());
            fold(&TransparentAddressReader::new(store.staged()), block, &mut changes);
            store.apply(changes);
            if height == 2 {
                store.commit().expect("SimFs commit");
            }
        }
        let budget = |rows: usize| {
            TransparentAddressReader::new(store.staged())
                .with_max_rows(NonZeroUsize::new(rows).expect("non-zero"))
        };
        let over = |rows: usize| Some(ServeError::TooManyRows { limit: rows });
        let both = [first, second];

        // `first` alone = 3 rows (2 committed + 1 buffered); both = 4
        let at = budget(3);
        assert_eq!(at.balance(&first).map(Zatoshis::as_u64), Ok(30));
        assert_eq!(at.utxos(&first, h(0)).map(|rows| rows.len()), Ok(3));
        assert_eq!(at.transactions(&first, h(0), h(3)).map(|found| found.len()), Ok(3));
        assert_eq!(at.balances(&both).err(), over(3), "3 + 1 across the request's addresses");

        let under = budget(2);
        assert_eq!(under.balance(&first).err(), over(2));
        assert_eq!(under.utxos(&first, h(0)).err(), over(2));
        let narrow = under.transactions(&first, h(3), h(3)).err();
        assert_eq!(narrow, over(2), "a narrow range still walks the whole history");
        assert_eq!(under.utxos(&first, h(2)).map(|rows| rows.len()), Ok(2), "from 2: 2 rows");

        let roomy = budget(4);
        let balances = roomy.balances(&both).map(|all| all.into_iter().map(Zatoshis::as_u64));
        let balances = balances.map(Vec::from_iter);
        assert_eq!(balances, Ok(vec![30, 10]));
    }
}
