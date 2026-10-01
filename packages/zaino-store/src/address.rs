//! Transparent address history over the finalised store.
//!
//! The finalised half holds history, so it answers the whole read: what an
//! address received, what of it is still unspent, and the balance changes in
//! between. The volatile window cannot — attributing a spend to an address needs
//! the output that outpoint created, which a bounded window does not have — so
//! only this tier implements [`AddressRead`], and the composer joins it with the
//! window's narrower answer.
//!
//! # Four indexes, because a spend is three lookups from a receive
//!
//! The address index is the receive side alone: address → the outputs that paid
//! it. Everything else is composed on read.
//!
//! ```text
//! receives(a)     → address_history, scanned by the address prefix
//! unspent(a)      → receives minus those in transparent_spends
//! spend location  → transparent_spends gives the spending txid,
//!                   txid_location gives its height and position,
//!                   transparent_data gives which of its inputs it was
//! ```
//!
//! A balance change is reported at the height and transaction that caused it, so
//! a spend's delta needs that location — which is why this read composes from
//! four indexes and [`local::AddressHistory`] names all four.
//!
//! # As of the watermark
//!
//! Every answer is bounded by the pinned watermark. An output this tier calls
//! unspent may have been spent in the window above it, which is the composer's
//! business and not this tier's.

use zaino_address::{script_paying, transparent_address_key};
use zaino_indexes::capabilities::local::{self, Backs};
use zaino_indexes::indexes::address_history::{read_receives, AddrId};
use zaino_indexes::indexes::transparent_data::{self, TransparentDataIndex};
use zaino_indexes::indexes::transparent_spends::{read_spender, OutpointKey};
use zaino_indexes::indexes::txid_location::{self, TxLocation, TxidLocationIndex};
use zaino_persistence::Backend;
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Height, HeightRange, OutputIndex, Script, SignedZatoshis,
    TransactionId, TransparentAddress, Utxo, Zatoshis, ZatoshisFlowSum,
};
use zaino_service::error::AddressReadError;
use zaino_service::AddressRead;

use crate::{read_index_value, read_keyed, StoreSnapshot};

/// One receive of the queried address, joined with where it was spent if the
/// finalised range spent it.
///
/// The join every method projects from, so one call costs one receive scan and
/// one spend resolution per receive rather than that much per method.
struct Entry {
    /// Height that mined the paying transaction.
    height: Height,
    /// The paying transaction.
    txid: TransactionId,
    /// Which of its outputs paid the address.
    output_index: OutputIndex,
    /// Amount received.
    value: Zatoshis,
    /// Where it was spent, if this tier saw a spend.
    spent: Option<SpendSite>,
}

/// Where a spend happened: the transaction, its block, and which input it was.
struct SpendSite {
    by: TransactionId,
    height: Height,
    block_index: u32,
    input_index: OutputIndex,
}

/// The store answers address history wherever its index set builds the four
/// indexes the capability composes from.
impl<B, M> AddressRead for StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Backs<local::AddressHistory>,
{
    async fn balance(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        let arrived: Vec<Entry> = self
            .entries(addr)?
            .into_iter()
            .filter(|entry| covers(range, entry.height))
            .collect();

        // Gross receipts count every output that arrived in the range, spent or
        // not: a flow total, so it is not supply-bounded.
        let received = ZatoshisFlowSum::try_accumulate(arrived.iter().map(|entry| entry.value))
            .ok_or_else(|| fatal("gross receipts overflowed"))?;

        // What is still held of them: the ones this tier saw no spend of.
        let balance = Zatoshis::sum_balances(
            arrived
                .iter()
                .filter(|entry| entry.spent.is_none())
                .map(|entry| entry.value),
        )
        .ok_or_else(|| fatal("balance exceeds the supply bound"))?;

        Ok(AddressBalance { balance, received })
    }

    async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        // Range-less by contract: every unspent output, whenever it arrived.
        let unspent: Vec<Entry> = self
            .entries(addr)?
            .into_iter()
            .filter(|entry| entry.spent.is_none())
            .collect();
        if unspent.is_empty() {
            return Ok(Vec::new());
        }

        // The script is reconstructed from the address rather than fetched: a
        // standard script is determined by the address it pays, and only a
        // standard script could have matched this address in the index. Having
        // receives at all establishes the address is transparent.
        let script = self.script_for(addr)?;
        Ok(unspent
            .into_iter()
            .map(|entry| Utxo {
                address: addr.clone(),
                txid: entry.txid,
                output_index: entry.output_index,
                script: script.clone(),
                satoshis: entry.value,
                height: entry.height,
            })
            .collect())
    }

    async fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        let mut deltas = Vec::new();
        for entry in self.entries(addr)? {
            if covers(range, entry.height) {
                deltas.push(AddressDelta {
                    satoshis: positive(entry.value)?,
                    txid: entry.txid,
                    index: entry.output_index,
                    height: entry.height,
                    address: addr.clone(),
                    block_index: self.block_index(entry.txid)?,
                });
            }
            // A spend is a delta at *its own* height, not the receive's, so the
            // range selects the two independently.
            if let Some(site) = entry.spent.filter(|site| covers(range, site.height)) {
                deltas.push(AddressDelta {
                    satoshis: negative(entry.value)?,
                    txid: site.by,
                    index: site.input_index,
                    height: site.height,
                    address: addr.clone(),
                    block_index: Some(site.block_index),
                });
            }
        }
        // The legacy node orders deltas by (height, block_index, index). The
        // receive scan is height ordered, but a spend lands at its own height, so
        // interleaving them needs the sort.
        deltas.sort_by_key(|delta| (u32::from(delta.height), delta.block_index, delta.index));
        Ok(deltas)
    }

    async fn tx_ids(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        // Every transaction that moved value for this address: the ones that
        // paid it and the ones that spent what it held. One transaction can do
        // both, and two receives can share one, so the result is deduplicated.
        let mut txids = Vec::new();
        for entry in self.entries(addr)? {
            if covers(range, entry.height) {
                txids.push((entry.height, entry.txid));
            }
            if let Some(site) = entry.spent.filter(|site| covers(range, site.height)) {
                txids.push((site.height, site.by));
            }
        }
        txids.sort_by_key(|(height, txid)| (u32::from(*height), <[u8; 32]>::from(*txid)));
        txids.dedup();
        Ok(txids.into_iter().map(|(_, txid)| txid).collect())
    }
}

impl<B, M> StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Backs<local::AddressHistory>,
{
    /// Every receive of `addr` at or below the watermark, each joined with where
    /// it was spent if this tier spent it.
    ///
    /// The one scan the whole read is built on. The index returns receives in
    /// height order and the join preserves it.
    fn entries(&self, addr: &TransparentAddress) -> Result<Vec<Entry>, AddressReadError> {
        let Some((script_type, hash)) = transparent_address_key(addr) else {
            // Not a transparent address, so nothing was ever keyed under it.
            // A well-formed question with an empty answer, not a failure.
            return Ok(Vec::new());
        };
        let reader = self.reader()?;
        let receives = read_receives(&reader, AddrId { script_type, hash })
            .map_err(|e| transient(format!("read address_history: {e}")))?;

        receives
            .into_iter()
            .map(|receive| {
                let outpoint = OutpointKey {
                    prev_txid: receive.txid,
                    prev_index: receive.output_index,
                };
                Ok(Entry {
                    height: domain_height(receive.height)?,
                    txid: receive.txid,
                    output_index: receive.output_index,
                    value: receive.value,
                    spent: self.spend_site(&reader, outpoint)?,
                })
            })
            .collect()
    }

    /// Where the finalised range spent `outpoint`, if it did.
    ///
    /// Three lookups: the spending transaction, its location, then which of its
    /// inputs consumed the outpoint. A recorded spend whose transaction cannot
    /// be located, or whose block carries no transparent entry, is index
    /// corruption rather than absence — the engine commits all four indexes in
    /// one batch — so it is reported rather than read as unspent.
    fn spend_site(
        &self,
        reader: &B::Reader,
        outpoint: OutpointKey,
    ) -> Result<Option<SpendSite>, AddressReadError> {
        let Some(by) = read_spender(reader, &outpoint)
            .map_err(|e| transient(format!("read transparent_spends: {e}")))?
        else {
            return Ok(None);
        };
        let location = self
            .location(reader, by)?
            .ok_or_else(|| fatal("a recorded spend's transaction has no location"))?;
        let height = domain_height(location.height)?;
        Ok(Some(SpendSite {
            by,
            height,
            block_index: location.tx_index,
            input_index: self.input_index(reader, &location, height, outpoint)?,
        }))
    }

    /// Which input of the transaction at `location` consumed `outpoint`.
    fn input_index(
        &self,
        reader: &B::Reader,
        location: &TxLocation,
        height: Height,
        outpoint: OutpointKey,
    ) -> Result<OutputIndex, AddressReadError> {
        let block = read_index_value::<TransparentDataIndex, B>(
            reader,
            transparent_data::ID.into(),
            height,
        )
        .map_err(|t| transient(t.0))?
        .ok_or_else(|| fatal("a spending transaction's block has no transparent data"))?;
        let index =
            usize::try_from(location.tx_index).map_err(|_| fatal("a tx index exceeds usize"))?;
        let tx = block
            .0
            .get(index)
            .ok_or_else(|| fatal("a spending transaction is past the end of its block"))?;
        let position = tx
            .inputs
            .iter()
            .position(|(prev_txid, prev_index)| {
                *prev_txid == outpoint.prev_txid && *prev_index == outpoint.prev_index
            })
            .ok_or_else(|| fatal("a recorded spender does not consume the outpoint"))?;
        OutputIndex::try_from(position).map_err(|_| fatal("an input index exceeds the wire limit"))
    }

    /// The canonical script paying `addr`.
    fn script_for(&self, addr: &TransparentAddress) -> Result<Script, AddressReadError> {
        script_paying(addr).ok_or_else(|| {
            // Unreachable through `entries`, which yields nothing for an address
            // that resolves to no key; named rather than silently defaulted.
            fatal("an address with receives resolves to no script")
        })
    }

    /// The position within its block of the transaction `txid`, if located.
    fn block_index(&self, txid: TransactionId) -> Result<Option<u32>, AddressReadError> {
        let reader = self.reader()?;
        Ok(self
            .location(&reader, txid)?
            .map(|location| location.tx_index))
    }

    /// Where `txid` was mined, per the location index.
    fn location(
        &self,
        reader: &B::Reader,
        txid: TransactionId,
    ) -> Result<Option<TxLocation>, AddressReadError> {
        read_keyed::<TxidLocationIndex, B>(reader, txid_location::ID.into(), &txid)
            .map_err(|t| transient(t.0))
    }

    /// A reader over the pinned backend.
    fn reader(&self) -> Result<B::Reader, AddressReadError> {
        self.backend
            .reader()
            .map_err(|e| fatal(format!("open reader: {e}")))
    }
}

/// Whether `range` includes `height`.
fn covers(range: HeightRange, height: Height) -> bool {
    range.start <= height && height <= range.end
}

/// The engine's height as the domain's, which is narrower.
fn domain_height(height: zaino_sync::primitives::BlockHeight) -> Result<Height, AddressReadError> {
    u32::try_from(height.value())
        .ok()
        .and_then(|h| Height::try_from(h).ok())
        .ok_or_else(|| fatal("an indexed height exceeds the protocol limit"))
}

/// A receive, as a positive balance change.
fn positive(value: Zatoshis) -> Result<SignedZatoshis, AddressReadError> {
    delta(value, 1)
}

/// A spend, as a negative balance change.
fn negative(value: Zatoshis) -> Result<SignedZatoshis, AddressReadError> {
    delta(value, -1)
}

/// `value` with `sign`, as a balance change.
fn delta(value: Zatoshis, sign: i64) -> Result<SignedZatoshis, AddressReadError> {
    i64::try_from(value.as_u64())
        .ok()
        .and_then(|magnitude| magnitude.checked_mul(sign))
        .and_then(|signed| SignedZatoshis::try_new(signed).ok())
        .ok_or_else(|| fatal("an amount is not a representable balance change"))
}

fn fatal(message: impl Into<String>) -> AddressReadError {
    AddressReadError::Fatal(message.into())
}

fn transient(message: impl Into<String>) -> AddressReadError {
    AddressReadError::Transient(message.into())
}
