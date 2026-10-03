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
use zaino_indexes::indexes::address_history::{read_receives, AddrId, ReceivesReadError};
use zaino_indexes::indexes::transparent_spends::OutpointKey;
use zaino_indexes::indexes::txid_location::{self, TxLocation, TxidLocationIndex};
use zaino_persistence::Backend;
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Height, HeightRange, OutputIndex, Script, SignedZatoshis,
    TransactionId, TransparentAddress, Utxo, Zatoshis, ZatoshisFlowSum,
};
use zaino_service::error::AddressReadError;
use zaino_service::{AddressRead, ReadBudget};
use zaino_sync::primitives::BlockHeight;

use crate::spend_resolve::{resolve_spend, ResolveError};
use crate::{read_keyed, StoreSnapshot};

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
        budget: &mut ReadBudget,
    ) -> Result<AddressBalance, AddressReadError> {
        // A balance counts only receives inside the range, and whether each is
        // spent (at any height) — never a spend's height — so the receive scan is
        // bounded to exactly `[range.start, range.end]`.
        let arrived: Vec<Entry> = self
            .entries(addr, receive_start(range), receive_end(range), budget)?
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
        budget: &mut ReadBudget,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        // Range-less by contract: every unspent output, whenever it arrived, so
        // the whole address prefix is read.
        let unspent: Vec<Entry> = self
            .entries(addr, whole_history_start(), whole_history_end(), budget)?
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
        budget: &mut ReadBudget,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        // A spend is a delta at its own height, and the receive it spends can lie
        // anywhere at or below that height — so the scan cannot be lower-bounded
        // by `range.start` without dropping an in-range spend of an earlier
        // receive. It is upper-bounded by `range.end`: a receive above the range
        // can contribute neither a receive delta (its height is out of range) nor
        // a spend delta (a spend is never below its receive).
        let mut deltas = Vec::new();
        for entry in self.entries(addr, whole_history_start(), receive_end(range), budget)? {
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
        budget: &mut ReadBudget,
    ) -> Result<Vec<(Option<Height>, TransactionId)>, AddressReadError> {
        // Every transaction that moved value for this address: the ones that
        // paid it and the ones that spent what it held. One transaction can do
        // both, and two receives can share one, so the result is deduplicated.
        // The local index knows each transaction's height, so every pair carries
        // `Some(height)`, letting a multi-address caller merge the unions by it.
        // Same bound as `deltas`: a spend in range can belong to a receive from
        // any earlier height, so the scan is only upper-bounded by `range.end`.
        let mut txids = Vec::new();
        for entry in self.entries(addr, whole_history_start(), receive_end(range), budget)? {
            if covers(range, entry.height) {
                txids.push((entry.height, entry.txid));
            }
            if let Some(site) = entry.spent.filter(|site| covers(range, site.height)) {
                txids.push((site.height, site.by));
            }
        }
        txids.sort_by_key(|(height, txid)| (u32::from(*height), <[u8; 32]>::from(*txid)));
        txids.dedup();
        Ok(txids
            .into_iter()
            .map(|(height, txid)| (Some(height), txid))
            .collect())
    }
}

impl<B, M> StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Backs<local::AddressHistory>,
{
    /// Every receive of `addr` with height in `[start, end]` (at or below the
    /// watermark), each joined with where it was spent if this tier spent it.
    ///
    /// The one scan the whole read is built on. The caller bounds the receive
    /// heights to exactly what its answer needs so the scan reads a contiguous
    /// address-prefixed slice, not the namespace. The index returns receives in
    /// height order and the join preserves it.
    ///
    /// `budget` is the request-scoped ceiling, charged entry by entry during the
    /// scan and shared across the addresses one query reads, so the request is
    /// bounded as a whole rather than per address.
    fn entries(
        &self,
        addr: &TransparentAddress,
        start: BlockHeight,
        end: BlockHeight,
        budget: &mut ReadBudget,
    ) -> Result<Vec<Entry>, AddressReadError> {
        let Some((script_type, hash)) = transparent_address_key(addr) else {
            // Not a transparent address, so nothing was ever keyed under it.
            // A well-formed question with an empty answer, not a failure.
            return Ok(Vec::new());
        };
        let reader = self.reader()?;
        let receives = read_receives(&reader, AddrId { script_type, hash }, start, end, budget)
            .map_err(|e| receives_read_error(addr, e))?;

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
    /// The shared [`resolve_spend`] walk — the spending transaction, its
    /// location, then which of its inputs consumed the outpoint — projected onto
    /// the [`SpendSite`] this read reports a delta at. A recorded spend whose
    /// transaction cannot be located, or whose block carries no transparent entry,
    /// is index corruption rather than absence — the engine commits all four
    /// indexes in one batch — so it is reported rather than read as unspent.
    fn spend_site(
        &self,
        reader: &B::Reader,
        outpoint: OutpointKey,
    ) -> Result<Option<SpendSite>, AddressReadError> {
        Ok(resolve_spend::<B>(reader, outpoint)
            .map_err(address_read_error)?
            .map(|resolved| SpendSite {
                by: resolved.by,
                height: resolved.height,
                block_index: resolved.block_index,
                input_index: resolved.input_index,
            }))
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

/// A protocol [`Height`] as the index's [`BlockHeight`], for a receive-scan bound.
fn block_height(height: Height) -> BlockHeight {
    BlockHeight::new(u64::from(height))
}

/// The receive-scan lower bound when receives below the range cannot matter —
/// the range's own start.
fn receive_start(range: HeightRange) -> BlockHeight {
    block_height(range.start)
}

/// The receive-scan upper bound: the range's end. Every read stops here, because
/// no receive above the asked range contributes to any of them.
fn receive_end(range: HeightRange) -> BlockHeight {
    block_height(range.end)
}

/// The lower bound of a whole-address scan: genesis.
fn whole_history_start() -> BlockHeight {
    BlockHeight::new(0)
}

/// The upper bound of a whole-address scan: the protocol height ceiling, above
/// which no block exists, so the scan covers every entry of the address.
fn whole_history_end() -> BlockHeight {
    BlockHeight::new(u64::from(u32::MAX))
}

/// Fold a receive-scan failure into this read's error, classified per variant.
///
/// A ceiling breach is a typed [`AddressReadError::TooLarge`] carrying the queried
/// address and the limit, so the adapter refuses that one request with a clear
/// message. The remaining cases are classified by what a retry would do, rather
/// than collapsed into one transient catch-all:
///
/// - A backend read failure (lock contention, a mid-swap race) may clear on a
///   retry, so it is [`transient`].
/// - A decode failure is index corruption: the scan read a persisted entry whose
///   bytes do not parse, and a retry re-reads the identical bytes and fails
///   identically. It is [`fatal`], so the caller stops rather than retrying a
///   read that cannot succeed.
/// - An out-of-range height bound cannot arise from a real request (block heights
///   are capped within `u32`, far below the `u64::MAX` that has no successor); it
///   would indicate a caller passing an impossible bound, so it is [`fatal`] too.
fn receives_read_error(addr: &TransparentAddress, error: ReceivesReadError) -> AddressReadError {
    match error {
        ReceivesReadError::TooLarge { limit } => AddressReadError::TooLarge {
            address: addr.clone(),
            limit,
        },
        ReceivesReadError::Backend(backend) => {
            transient(format!("read address_history: {backend}"))
        }
        ReceivesReadError::Decode(decode) => {
            fatal(format!("decode address_history entry: {decode}"))
        }
        ReceivesReadError::HeightOutOfRange { height } => {
            fatal(format!("address_history scan bound out of range: {height}"))
        }
    }
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

/// Fold a shared spend-resolution failure into this read's error, preserving its
/// transient/fatal classification.
fn address_read_error(error: ResolveError) -> AddressReadError {
    match error {
        ResolveError::Transient(message) => transient(message),
        ResolveError::Fatal(message) => fatal(message),
    }
}

fn fatal(message: impl Into<String>) -> AddressReadError {
    AddressReadError::Fatal(message.into())
}

fn transient(message: impl Into<String>) -> AddressReadError {
    AddressReadError::Transient(message.into())
}

#[cfg(test)]
mod tests {
    use super::receives_read_error;
    use zaino_indexes::indexes::address_history::ReceivesReadError;
    use zaino_primitives::types::TransparentAddress;
    use zaino_service::error::AddressReadError;

    /// A ceiling breach maps to the typed `TooLarge` refusal carrying the queried
    /// address and the limit — the one request fails, with a message an operator
    /// can read, rather than becoming an opaque transient.
    #[test]
    fn a_ceiling_breach_maps_to_a_typed_too_large_refusal() {
        let addr = TransparentAddress::new("t1exampleaddress".to_owned());
        let mapped = receives_read_error(&addr, ReceivesReadError::TooLarge { limit: 7 });
        match mapped {
            AddressReadError::TooLarge { address, limit } => {
                assert_eq!(address, addr);
                assert_eq!(limit, 7);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    /// A decode failure is index corruption, not a race: a retry re-reads the
    /// same unparseable bytes, so it maps to a fatal read failure rather than a
    /// transient one.
    #[test]
    fn a_decode_failure_is_fatal() {
        let addr = TransparentAddress::new("t1exampleaddress".to_owned());
        let mapped = receives_read_error(
            &addr,
            ReceivesReadError::Decode(zaino_persistence_codec::DecodeError::Invalid(
                "bad bytes".to_owned(),
            )),
        );
        assert!(matches!(mapped, AddressReadError::Fatal(_)));
    }

    /// A backend read failure may clear on a retry, so it stays transient.
    #[test]
    fn a_backend_failure_stays_transient() {
        let addr = TransparentAddress::new("t1exampleaddress".to_owned());
        let mapped = receives_read_error(
            &addr,
            ReceivesReadError::Backend(zaino_sync::backend::ReadError::NamespaceNotFound(
                "address_history".to_owned(),
            )),
        );
        assert!(matches!(mapped, AddressReadError::Transient(_)));
    }
}
