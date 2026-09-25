//! Transparent address history, per placement.
//!
//! One [`AddressRead`] impl on the snapshot, dispatching through
//! [`AddressPlacement`] — a trait implemented **on the placement marker**,
//! once for [`Remote`] and once for [`Local`]. The two impls have different
//! `Self` types, so they cannot overlap; which one a use case gets is its
//! routing's `Address` type, and a handler bound on [`AddressRead`] never
//! learns which. [`Withheld`](zaino_service::routing::Withheld) has no impl,
//! so under a routing that withholds address history the read does not exist.
//!
//! **Remote** relays each read live to the validator. It discloses the queried
//! addresses to it — the privacy cost a local transparent index exists to
//! remove — and the validator's balance is range-less, so the caller's range
//! is not honoured there.
//!
//! **Local** merges across the seam: the finalised store answers heights up to
//! the watermark, the head answers the volatile window above it, and the two
//! halves are joined. Requires both tiers to have an address read, and the
//! head to have a spend read (a store UTXO may have been spent in the window).

use std::future::Future;

use zaino_chainview::ChainViewSnapshot;
use zaino_core::{
    AddressBalance, AddressDelta, HeightRange, Outpoint, SpendStatus, TransactionId,
    TransparentAddress, Utxo, Zatoshis, ZatoshisFlowSum,
};
use zaino_service::error::{AddressReadError, SpendReadError};
use zaino_service::routing::{Local, Remote, Routing};
use zaino_service::{AddressRead, ChainSegment, CompactBlockRead, SpendRead};
use zaino_source::{GetAddressBalance, GetAddressDeltas, GetAddressTxids, GetAddressUtxos};

use super::snapshot::split_at_seam;
use super::ComposedSnapshot;
use crate::remote::RemoteChainView;

/// How a placement answers address history over the providers `(F, N, Src)`.
///
/// Implemented on the placement marker, not on the snapshot: that is what lets
/// `Local` and `Remote` each carry their own provider bounds without the two
/// impls overlapping.
pub trait AddressPlacement<F, N, Src>: Send + Sync + 'static {
    fn balance(
        local: &ChainViewSnapshot<F, N>,
        remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<AddressBalance, AddressReadError>> + Send;

    fn unspent_outpoints(
        local: &ChainViewSnapshot<F, N>,
        remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<Vec<Utxo>, AddressReadError>> + Send;

    fn deltas(
        local: &ChainViewSnapshot<F, N>,
        remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<AddressDelta>, AddressReadError>> + Send;

    fn tx_ids(
        local: &ChainViewSnapshot<F, N>,
        remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<TransactionId>, AddressReadError>> + Send;
}

/// The one impl a handler sees: dispatch on the routing's placement.
impl<F, N, Src, R> AddressRead for ComposedSnapshot<F, N, Src, R>
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
    Src: Send + Sync + 'static,
    R: Routing,
    R::Address: AddressPlacement<F, N, Src>,
{
    async fn balance(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        R::Address::balance(self.local(), self.remote(), addr, range).await
    }

    async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        R::Address::unspent_outpoints(self.local(), self.remote(), addr).await
    }

    async fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        R::Address::deltas(self.local(), self.remote(), addr, range).await
    }

    async fn tx_ids(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        R::Address::tx_ids(self.local(), self.remote(), addr, range).await
    }
}

// --- Remote -------------------------------------------------------------------

impl<F, N, Src> AddressPlacement<F, N, Src> for Remote
where
    F: ChainSegment + CompactBlockRead,
    N: ChainSegment + CompactBlockRead,
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
        remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        _range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        // `getaddressbalance` is range-less: this is the balance as of the
        // validator's tip, whatever range was asked for.
        remote.balance(addr).await
    }

    async fn unspent_outpoints(
        _local: &ChainViewSnapshot<F, N>,
        remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        remote.unspent_outpoints(addr).await
    }

    async fn deltas(
        _local: &ChainViewSnapshot<F, N>,
        remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        remote.deltas(addr, range).await
    }

    async fn tx_ids(
        _local: &ChainViewSnapshot<F, N>,
        remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        remote.tx_ids(addr, range).await
    }
}

// --- Local --------------------------------------------------------------------

/// Join two balances over disjoint runs of blocks. Overflow is reported, not
/// saturated: a wrong total must never be served as a right one.
fn join_balances(a: AddressBalance, b: AddressBalance) -> Result<AddressBalance, AddressReadError> {
    let balance = a
        .balance
        .checked_add(b.balance)
        .ok_or_else(|| AddressReadError::Fatal("balance exceeds the supply bound".to_owned()))?;
    let received = a
        .received
        .checked_join(b.received)
        .ok_or_else(|| AddressReadError::Fatal("received total overflowed".to_owned()))?;
    Ok(AddressBalance { balance, received })
}

/// The balance of nothing: what an empty half of a split range contributes.
fn empty_balance() -> Result<AddressBalance, AddressReadError> {
    let balance = Zatoshis::sum_balances(core::iter::empty())
        .ok_or_else(|| AddressReadError::Fatal("an empty sum overflowed".to_owned()))?;
    let received = ZatoshisFlowSum::try_accumulate(core::iter::empty())
        .ok_or_else(|| AddressReadError::Fatal("an empty sum overflowed".to_owned()))?;
    Ok(AddressBalance { balance, received })
}

/// A spend-status failure met while filtering UTXOs is an address-read failure
/// of the same kind.
fn spend_to_address_error(error: SpendReadError) -> AddressReadError {
    match error {
        SpendReadError::NotServiceable(capability) => AddressReadError::NotServiceable(capability),
        SpendReadError::Transient(message) => AddressReadError::Transient(message),
        SpendReadError::Fatal(message) => AddressReadError::Fatal(message),
    }
}

impl<F, N, Src> AddressPlacement<F, N, Src> for Local
where
    F: ChainSegment + CompactBlockRead + AddressRead,
    N: ChainSegment + CompactBlockRead + AddressRead + SpendRead,
    Src: Send + Sync + 'static,
{
    async fn balance(
        local: &ChainViewSnapshot<F, N>,
        _remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<AddressBalance, AddressReadError> {
        let (fs, nfs) = split_at_seam(local, range);
        let fs = match fs {
            Some(range) => local.finalised().balance(addr, range).await?,
            None => empty_balance()?,
        };
        let nfs = match nfs {
            Some(range) => local.non_finalised().balance(addr, range).await?,
            None => empty_balance()?,
        };
        join_balances(fs, nfs)
    }

    async fn unspent_outpoints(
        local: &ChainViewSnapshot<F, N>,
        _remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        // A store UTXO is unspent as of the watermark; the window above it may
        // have spent it since. Keep it only if the head does not know a spend.
        // Outputs the head created are unspent by the head's own account.
        let head = local.non_finalised();
        let mut unspent = Vec::new();
        for utxo in local.finalised().unspent_outpoints(addr).await? {
            let outpoint = Outpoint {
                txid: utxo.txid,
                index: utxo.output_index,
            };
            match head.spend_status(outpoint).await {
                Ok(SpendStatus::Spent { .. } | SpendStatus::SpentSpenderUnknown) => {}
                Ok(SpendStatus::Unspent | SpendStatus::NoSuchOutput) => unspent.push(utxo),
                Err(error) => return Err(spend_to_address_error(error)),
            }
        }
        unspent.extend(head.unspent_outpoints(addr).await?);
        Ok(unspent)
    }

    async fn deltas(
        local: &ChainViewSnapshot<F, N>,
        _remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        let (fs, nfs) = split_at_seam(local, range);
        let mut deltas = Vec::new();
        if let Some(range) = fs {
            deltas.extend(local.finalised().deltas(addr, range).await?);
        }
        if let Some(range) = nfs {
            deltas.extend(local.non_finalised().deltas(addr, range).await?);
        }
        Ok(deltas)
    }

    async fn tx_ids(
        local: &ChainViewSnapshot<F, N>,
        _remote: &RemoteChainView<Src>,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        // The halves cover disjoint heights and a transaction is mined once, so
        // concatenation is the union.
        let (fs, nfs) = split_at_seam(local, range);
        let mut txids = Vec::new();
        if let Some(range) = fs {
            txids.extend(local.finalised().tx_ids(addr, range).await?);
        }
        if let Some(range) = nfs {
            txids.extend(local.non_finalised().tx_ids(addr, range).await?);
        }
        Ok(txids)
    }
}
