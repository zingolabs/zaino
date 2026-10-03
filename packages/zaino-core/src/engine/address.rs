//! Transparent address history, per placement.
//!
//! One [`AddressRead`] impl on the snapshot, dispatching through
//! [`AddressPlacement`] — a trait implemented **on the placement marker**,
//! once for [`Passthrough`] and once for [`Local`]. The two impls have different
//! `Self` types, so they cannot overlap; which one a use case gets is its
//! routing's `Address` type, and a handler bound on [`AddressRead`] never
//! learns which. [`Withheld`](zaino_core::routing::Withheld) has no impl,
//! so under a routing that withholds address history the read does not exist.
//!
//! **Passthrough** relays each read live to the validator. It discloses the queried
//! addresses to it — the privacy cost a local transparent index exists to
//! remove — and the validator's balance is range-less, so the caller's range
//! is not honoured there.
//!
//! **Local** merges across the seam: the finalised store answers heights up to
//! the watermark, the head answers the volatile window above it, and the two
//! halves are joined. Requires both tiers to have an address read, and the
//! head to have a spend read (a store UTXO may have been spent in the window).

use std::collections::HashMap;
use std::future::Future;

use crate::chain_view::ChainTier;
use crate::chain_view::ChainViewSnapshot;
use crate::routing::{Local, Passthrough, Routing};
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Height, HeightRange, Outpoint, SignedZatoshis, TransactionId,
    TransparentAddress, TransparentReceive, TransparentSpend, Utxo, Zatoshis, ZatoshisFlowSum,
};
use zaino_service::error::AddressReadError;
use zaino_service::{AddressRead, AddressReceiveRead};
use zaino_source::{GetAddressBalance, GetAddressDeltas, GetAddressTxids, GetAddressUtxos};

use super::EngineSnapshot;
use super::snapshot::split_at_seam;
use crate::passthrough::PassthroughProvider;

/// How a placement answers address history over the providers `(F, N, Src)`.
///
/// Implemented on the placement marker, not on the snapshot: that is what lets
/// `Local` and `Passthrough` each carry their own provider bounds without the two
/// impls overlapping.
pub trait AddressPlacement<F, N, Src>: Send + Sync + 'static {
    fn balance(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<AddressBalance, AddressReadError>> + Send;

    fn unspent_outpoints(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<Vec<Utxo>, AddressReadError>> + Send;

    fn deltas(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<AddressDelta>, AddressReadError>> + Send;

    fn tx_ids(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<(Option<Height>, TransactionId)>, AddressReadError>> + Send;
}

/// The one impl a handler sees: dispatch on the routing's placement.
impl<F, N, Src, R> AddressRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Send + Sync + 'static,
    R: Routing,
    R::Address: AddressPlacement<F, N, Src>,
{
    async fn balance(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        R::Address::balance(self.local(), self.passthrough(), addr, range).await
    }

    async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        R::Address::unspent_outpoints(self.local(), self.passthrough(), addr).await
    }

    async fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        R::Address::deltas(self.local(), self.passthrough(), addr, range).await
    }

    async fn tx_ids(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<(Option<Height>, TransactionId)>, AddressReadError> {
        R::Address::tx_ids(self.local(), self.passthrough(), addr, range).await
    }
}

// --- Passthrough -------------------------------------------------------------------

impl<F, N, Src> AddressPlacement<F, N, Src> for Passthrough
where
    F: ChainTier,
    N: ChainTier,
    Src: GetAddressBalance
        + GetAddressUtxos
        + GetAddressTxids
        + GetAddressDeltas
        + Send
        + Sync
        + 'static,
{
    async fn balance(
        _local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        // `getaddressbalance` is range-less: this is the balance as of the
        // validator's tip, whatever range was asked for.
        passthrough.balance(addr).await
    }

    async fn unspent_outpoints(
        _local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        passthrough.unspent_outpoints(addr).await
    }

    async fn deltas(
        _local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        passthrough.deltas(addr, range).await
    }

    async fn tx_ids(
        _local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<(Option<Height>, TransactionId)>, AddressReadError> {
        // The validator's `getaddresstxids` returns bare txids with no per-txid
        // height, so each pair's height is `None` — the honest "location unknown",
        // never a fabricated value. The only consumer of this placement is the
        // single-address light-wallet `GetTaddressTxids`, which drops the height;
        // the multi-address merge that needs real heights
        // (`queries::address_txids`) runs only under the `Local` placement.
        Ok(passthrough
            .tx_ids(addr, range)
            .await?
            .into_iter()
            .map(|txid| (None, txid))
            .collect())
    }
}

// --- Local --------------------------------------------------------------------

/// The balance of nothing: what an empty half of a split range contributes.
fn empty_balance() -> Result<AddressBalance, AddressReadError> {
    let balance = Zatoshis::sum_balances(core::iter::empty())
        .ok_or_else(|| AddressReadError::Fatal("an empty sum overflowed".to_owned()))?;
    let received = ZatoshisFlowSum::try_accumulate(core::iter::empty())
        .ok_or_else(|| AddressReadError::Fatal("an empty sum overflowed".to_owned()))?;
    Ok(AddressBalance { balance, received })
}

fn fatal(message: impl Into<String>) -> AddressReadError {
    AddressReadError::Fatal(message.into())
}

/// Whether `half` includes `height` — `false` when the half is absent.
fn in_range(half: Option<HeightRange>, height: Height) -> bool {
    half.is_some_and(|range| range.start <= height && height <= range.end)
}

/// Which way a balance change goes.
enum Sign {
    Received,
    Spent,
}

/// `value` as a balance change in the given direction.
fn signed(value: Zatoshis, sign: Sign) -> Result<SignedZatoshis, AddressReadError> {
    let magnitude =
        i64::try_from(value.as_u64()).map_err(|_| fatal("an amount exceeds a balance change"))?;
    let signed = match sign {
        Sign::Received => Some(magnitude),
        Sign::Spent => magnitude.checked_neg(),
    }
    .ok_or_else(|| fatal("an amount is not a representable balance change"))?;
    SignedZatoshis::try_new(signed).map_err(|_| fatal("a balance change is out of range"))
}

/// What the window contributed to an address over one range: the outputs it saw
/// paid, and the spends it saw of outpoints the address owned.
///
/// The composer builds this once per read and projects every answer from it,
/// because both halves come from the same two questions.
struct WindowPart {
    receives: Vec<TransparentReceive>,
    spends: Vec<TransparentSpend>,
}

impl WindowPart {
    /// Whether the window spent `outpoint`.
    fn spent(&self, outpoint: Outpoint) -> bool {
        self.spends.iter().any(|spend| spend.outpoint == outpoint)
    }
}

/// Ask the window what it saw of `addr` over `range`.
///
/// Two questions, and the second is phrased in terms the window can answer. It
/// cannot say which outpoints belong to the address — that needs the outputs
/// that created them, which it does not hold — so the composer supplies the
/// candidates: every outpoint the store still held entering the window, plus
/// every one the window itself paid the address.
///
/// The candidate set is complete. An outpoint the address owned and that was
/// spent at or below the watermark is absent from the store's unspent set, and
/// cannot be spent again; one spent inside the window is still in that set,
/// because the store has not seen the spend. So every outpoint of this address
/// that the window could spend is a candidate.
/// The two ranges are separate because the questions are.
///
/// Which receives count is bounded by what the caller asked about: a receive is
/// an event at its own height. Which spends count is not always: a *balance* is
/// what is held now, so a receive inside the asked range is spent if anything in
/// the window spent it, whether or not the asked range reaches that far. A
/// *delta* is an event too, so there the spend range is the asked one as well.
/// Passing both explicitly keeps each caller's choice visible.
async fn window_part<F, N>(
    local: &ChainViewSnapshot<F, N>,
    addr: &TransparentAddress,
    receives_in: Option<HeightRange>,
    spends_in: Option<HeightRange>,
    held: &[Utxo],
) -> Result<WindowPart, AddressReadError>
where
    F: ChainTier + AddressRead,
    N: ChainTier + AddressReceiveRead,
{
    let window = local.non_finalised();
    let receives = match receives_in {
        Some(range) => window.receives(addr, range).await?,
        None => Vec::new(),
    };

    let Some(spends_in) = spends_in else {
        return Ok(WindowPart {
            receives,
            spends: Vec::new(),
        });
    };
    let candidates: Vec<Outpoint> = held
        .iter()
        .map(|utxo| Outpoint {
            txid: utxo.txid,
            index: utxo.output_index,
        })
        .chain(receives.iter().map(|receive| Outpoint {
            txid: receive.txid,
            index: receive.output_index,
        }))
        .collect();
    let spends = window.spends(&candidates, spends_in).await?;
    Ok(WindowPart { receives, spends })
}

/// The value each candidate outpoint carried, for turning a spend into a delta.
///
/// A spend's magnitude is the value of the output it consumed, which the spender
/// does not carry: it comes from whichever side created the outpoint.
fn values(held: &[Utxo], receives: &[TransparentReceive]) -> HashMap<Outpoint, Zatoshis> {
    held.iter()
        .map(|utxo| {
            (
                Outpoint {
                    txid: utxo.txid,
                    index: utxo.output_index,
                },
                utxo.satoshis,
            )
        })
        .chain(receives.iter().map(|receive| {
            (
                Outpoint {
                    txid: receive.txid,
                    index: receive.output_index,
                },
                receive.value,
            )
        }))
        .collect()
}

impl<F, N, Src> AddressPlacement<F, N, Src> for Local
where
    F: ChainTier + AddressRead,
    N: ChainTier + AddressReceiveRead,
    Src: Send + Sync + 'static,
{
    async fn balance(
        local: &ChainViewSnapshot<F, N>,
        _passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        let (fs, nfs) = split_at_seam(local, range);
        let store = local.finalised();

        let fs_balance = match fs {
            Some(range) => store.balance(addr, range).await?,
            None => empty_balance()?,
        };
        // A balance is what is held *now*, so a receive inside the asked range
        // counts as spent if anything in the window spent it — even when the
        // asked range stops at or below the watermark. That matches the store's
        // own half, which nets every spend it saw rather than only those inside
        // the asked range.
        let held = store.unspent_outpoints(addr).await?;
        let part = window_part(local, addr, nfs, local.non_finalised().coverage(), &held).await?;

        // Gross receipts are additive: the halves cover disjoint heights, and a
        // receive is counted where it arrived.
        let received = fs_balance
            .received
            .checked_join(
                ZatoshisFlowSum::try_accumulate(part.receives.iter().map(|r| r.value))
                    .ok_or_else(|| fatal("the window's gross receipts overflowed"))?,
            )
            .ok_or_else(|| fatal("gross receipts overflowed"))?;

        // What is still held is summed, never subtracted: every output received
        // in range that neither tier has spent. The store's own netting covers
        // spends at or below the watermark; the window's covers the rest.
        let from_store = held
            .iter()
            .filter(|utxo| in_range(fs, utxo.height))
            .filter(|utxo| {
                !part.spent(Outpoint {
                    txid: utxo.txid,
                    index: utxo.output_index,
                })
            })
            .map(|utxo| utxo.satoshis);
        let from_window = part
            .receives
            .iter()
            .filter(|receive| {
                !part.spent(Outpoint {
                    txid: receive.txid,
                    index: receive.output_index,
                })
            })
            .map(|receive| receive.value);
        let balance = Zatoshis::sum_balances(from_store.chain(from_window))
            .ok_or_else(|| fatal("balance exceeds the supply bound"))?;

        Ok(AddressBalance { balance, received })
    }

    async fn unspent_outpoints(
        local: &ChainViewSnapshot<F, N>,
        _passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        // Range-less by contract, so the window's whole coverage is in scope.
        // A store output is unspent as of the watermark; the window above it may
        // have spent it since, and an output the window paid is unspent unless
        // the window itself spent it.
        let held = local.finalised().unspent_outpoints(addr).await?;
        let Some(window_range) = local.non_finalised().coverage() else {
            return Ok(held);
        };
        let part = window_part(local, addr, Some(window_range), Some(window_range), &held).await?;

        let mut unspent: Vec<Utxo> = held
            .into_iter()
            .filter(|utxo| {
                !part.spent(Outpoint {
                    txid: utxo.txid,
                    index: utxo.output_index,
                })
            })
            .collect();
        unspent.extend(
            part.receives
                .iter()
                .filter(|receive| {
                    !part.spent(Outpoint {
                        txid: receive.txid,
                        index: receive.output_index,
                    })
                })
                .cloned()
                .map(|receive| receive.into_utxo(addr.clone())),
        );
        Ok(unspent)
    }

    async fn deltas(
        local: &ChainViewSnapshot<F, N>,
        _passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        let (fs, nfs) = split_at_seam(local, range);
        let store = local.finalised();

        // The store reports its own half whole, spends included: it has the
        // history to attribute them.
        let mut deltas = match fs {
            Some(range) => store.deltas(addr, range).await?,
            None => Vec::new(),
        };

        let Some(nfs) = nfs else {
            return Ok(deltas);
        };
        // Both ranges are the asked one: a delta is an event, so a spend
        // outside the asked range is not one of its deltas.
        let held = store.unspent_outpoints(addr).await?;
        let part = window_part(local, addr, Some(nfs), Some(nfs), &held).await?;
        let values = values(&held, &part.receives);

        for receive in &part.receives {
            deltas.push(AddressDelta {
                satoshis: signed(receive.value, Sign::Received)?,
                txid: receive.txid,
                index: receive.output_index,
                height: receive.height,
                address: addr.clone(),
                // The window reports no transaction position; `None` says so
                // rather than substituting iteration order.
                block_index: None,
            });
        }
        for spend in &part.spends {
            let value = values
                .get(&spend.outpoint)
                .copied()
                .ok_or_else(|| fatal("a spend was reported for an outpoint not asked about"))?;
            deltas.push(AddressDelta {
                satoshis: signed(value, Sign::Spent)?,
                txid: spend.by,
                index: spend.input_index,
                height: spend.height,
                address: addr.clone(),
                block_index: None,
            });
        }
        Ok(deltas)
    }

    async fn tx_ids(
        local: &ChainViewSnapshot<F, N>,
        _passthrough: &PassthroughProvider<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<(Option<Height>, TransactionId)>, AddressReadError> {
        let (fs, nfs) = split_at_seam(local, range);
        let store = local.finalised();

        // The store's half is already `(Some(height), txid)`-sorted and
        // deduplicated — the local index knows every height.
        let mut txids = match fs {
            Some(range) => store.tx_ids(addr, range).await?,
            None => Vec::new(),
        };

        let Some(nfs) = nfs else {
            return Ok(txids);
        };
        // As for deltas: a transaction appears because of what it did inside the
        // asked range.
        let held = store.unspent_outpoints(addr).await?;
        let part = window_part(local, addr, Some(nfs), Some(nfs), &held).await?;

        // Every transaction that moved value for the address in the window: the
        // ones that paid it (at the receive's height), and the ones that spent
        // what it held (at the spend's height) — both heights known, so `Some`.
        // One can do both, so the window's contribution is sorted and
        // deduplicated before it is appended; the halves cover disjoint heights
        // (store <= watermark < window), so the concatenation stays height-ordered
        // and no transaction spans both.
        let mut from_window: Vec<(Height, TransactionId)> = part
            .receives
            .iter()
            .map(|receive| (receive.height, receive.txid))
            .chain(part.spends.iter().map(|spend| (spend.height, spend.by)))
            .collect();
        from_window.sort_by_key(|(height, txid)| (u32::from(*height), <[u8; 32]>::from(*txid)));
        from_window.dedup();
        txids.extend(
            from_window
                .into_iter()
                .map(|(height, txid)| (Some(height), txid)),
        );
        Ok(txids)
    }
}
