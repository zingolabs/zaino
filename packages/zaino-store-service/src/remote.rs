//! `RemoteChainView` — the passthrough provider.
//!
//! One half of the local/passthrough split: the validator answered **live**,
//! through the **canonical** `zaino-source` ports. It carries exactly the
//! capabilities the validator provides directly (broadcast today; treestate /
//! transaction / mempool / tip next), and never the ones zaino composes or
//! indexes itself — those belong to the local view (`ChainView`).
//!
//! It binds the canonical (resilient) traits, not the raw `OneShot*` ports: the
//! one-shots belong to the adapters that implement them and the
//! [`ValidatorClient`](zaino_source::ValidatorClient) decorator. A consumer binds
//! the twin, so resilience is already applied and this crate never touches a
//! single-attempt port. Which capabilities live here versus on the local view is
//! the classification — expressed as which provider carries the trait, and
//! checked where the two are composed (the engine and its coverage assertion).
//!
//! Reads answered here are **live, not pinned** to a snapshot's tip — inherent
//! to passthrough (the validator's state cannot be pinned to our view). That is
//! sound for the immutable, historical data light clients query.

use zaino_core::{
    AddressBalance, AddressDelta, BlockId, Height, HeightRange, RawTransaction, ShieldedPool,
    SubtreeRoot, TransactionId, TransparentAddress, Treestate, Utxo,
};
use zaino_service::error::{
    AddressReadError, BroadcastRejection, MempoolReadError, TreestateReadError, TxReadError,
};
use zaino_source::{
    GetAddressBalance, GetAddressBalanceError, GetAddressDeltas, GetAddressDeltasError,
    GetAddressTxids, GetAddressTxidsError, GetAddressUtxos, GetAddressUtxosError,
    GetMempoolSourceTip, GetMempoolTxids, GetMempoolTxidsError, GetRawMempoolTransaction,
    GetRawMempoolTransactionError, GetSubtreeRoots, GetSubtreeRootsError, GetTransaction,
    GetTransactionError, GetTreestate, GetTreestateError, SendRawTransaction,
    SendRawTransactionError, SourceError, TransactionResponse,
};

/// The passthrough provider over a resilient source handle `Src`.
pub struct RemoteChainView<Src> {
    source: Src,
}

impl<Src: Clone> Clone for RemoteChainView<Src> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
        }
    }
}

impl<Src> RemoteChainView<Src> {
    /// Wrap a resilient source handle as the passthrough provider.
    pub fn new(source: Src) -> Self {
        Self { source }
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: SendRawTransaction,
{
    /// Relay a wallet's transaction to the validator's send port — the only
    /// thing that can broadcast. Domain rejections map to the serving rejection;
    /// a transport failure surfaces as `Invalid` with the cause (a dedicated
    /// transient arm on the serving error surface is a follow-up).
    pub(crate) async fn broadcast(
        &self,
        raw_tx: Vec<u8>,
    ) -> Result<TransactionId, BroadcastRejection> {
        match self.source.send_raw_transaction(raw_tx).await {
            Ok(txid) => Ok(txid),
            Err(SourceError::Domain(SendRawTransactionError::Malformed(reason))) => {
                Err(BroadcastRejection::Malformed(reason))
            }
            Err(SourceError::Domain(SendRawTransactionError::Rejected(reason))) => {
                Err(BroadcastRejection::Invalid(reason))
            }
            Err(SourceError::NonDomain(cause)) => Err(BroadcastRejection::Invalid(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(BroadcastRejection::Invalid(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

/// Map a source failure on an address read to the read surface. Each address
/// port supplies its own domain mapping (its rejections differ); the transport
/// arms are shared — an unreachable validator or an unusable answer is transient
/// on every one of them.
fn address_failure<E: std::fmt::Debug + std::fmt::Display>(
    err: SourceError<E>,
    domain: impl FnOnce(E) -> AddressReadError,
) -> AddressReadError {
    match err {
        SourceError::Domain(e) => domain(e),
        SourceError::NonDomain(cause) => {
            AddressReadError::Transient(format!("validator unavailable: {cause}"))
        }
        SourceError::Unavailable(cause) => {
            AddressReadError::Transient(format!("validator unavailable: {cause}"))
        }
    }
}

/// Convert the read surface's half-open `[start, end)` [`HeightRange`] to the
/// inclusive `[start, last]` bounds the address source ports take. Returns
/// `None` for an empty range, so the caller answers empty without troubling the
/// validator.
fn inclusive_bounds(range: HeightRange) -> Option<(Height, Height)> {
    let last = range.end.checked_sub(1)?;
    (range.start <= last).then_some((range.start, last))
}

impl<Src> RemoteChainView<Src>
where
    Src: GetAddressBalance,
{
    /// The transparent balance of `addr`, live from the validator.
    ///
    /// Stopgap: `getaddressbalance` is range-less, so the caller's requested
    /// [`HeightRange`] cannot be honoured — this answers the balance as of the
    /// validator's tip. A range-scoped balance is a reason to move address reads
    /// local. Passing transparent addresses to the validator also discloses them,
    /// the privacy cost a local transparent index exists to remove.
    pub(crate) async fn balance(
        &self,
        addr: &TransparentAddress,
    ) -> Result<AddressBalance, AddressReadError> {
        self.source
            .get_address_balance(vec![addr.as_str().to_owned()])
            .await
            .map_err(|err| {
                address_failure(err, |GetAddressBalanceError::InvalidAddress(a)| {
                    AddressReadError::Fatal(format!("invalid address: {a}"))
                })
            })
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: GetAddressUtxos,
{
    /// The unspent transparent outputs of `addr`, live from the validator. See
    /// [`balance`](Self::balance) for the address-disclosure privacy cost.
    pub(crate) async fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> Result<Vec<Utxo>, AddressReadError> {
        self.source
            .get_address_utxos(vec![addr.as_str().to_owned()])
            .await
            .map_err(|err| {
                address_failure(err, |GetAddressUtxosError::InvalidAddress(a)| {
                    AddressReadError::Fatal(format!("invalid address: {a}"))
                })
            })
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: GetAddressTxids,
{
    /// The txids touching `addr` over `range`, live from the validator. See
    /// [`balance`](Self::balance) for the address-disclosure privacy cost.
    pub(crate) async fn tx_ids(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransactionId>, AddressReadError> {
        let Some((start, end)) = inclusive_bounds(range) else {
            return Ok(Vec::new());
        };
        self.source
            .get_address_txids(vec![addr.as_str().to_owned()], start, end)
            .await
            .map_err(|err| {
                address_failure(err, |domain| match domain {
                    GetAddressTxidsError::InvalidAddress(a) => {
                        AddressReadError::Fatal(format!("invalid address: {a}"))
                    }
                    GetAddressTxidsError::InvalidRange { start, end } => AddressReadError::Fatal(
                        format!("unserviceable height range {start}..={end}"),
                    ),
                })
            })
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: GetAddressDeltas,
{
    /// The balance deltas for `addr` over `range`, live from the validator. See
    /// [`balance`](Self::balance) for the address-disclosure privacy cost.
    pub(crate) async fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<AddressDelta>, AddressReadError> {
        let Some((start, end)) = inclusive_bounds(range) else {
            return Ok(Vec::new());
        };
        self.source
            .get_address_deltas(vec![addr.as_str().to_owned()], start, end)
            .await
            .map_err(|err| {
                address_failure(err, |domain| match domain {
                    GetAddressDeltasError::InvalidAddress(a) => {
                        AddressReadError::Fatal(format!("invalid address: {a}"))
                    }
                    GetAddressDeltasError::InvalidRange { start, end } => AddressReadError::Fatal(
                        format!("unserviceable height range {start}..={end}"),
                    ),
                })
            })
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: GetTreestate,
{
    /// The commitment treestate at `at`, live from the validator. Zaino does not
    /// index treestate, so this is passthrough. A height with no treestate is a
    /// definitive (non-retryable) answer; a transport failure is transient.
    pub(crate) async fn treestate(&self, at: Height) -> Result<Treestate, TreestateReadError> {
        match self.source.get_treestate(at).await {
            Ok(treestate) => Ok(treestate),
            Err(SourceError::Domain(GetTreestateError::HeightNotFound(height))) => Err(
                TreestateReadError::Fatal(format!("no treestate at height {height}")),
            ),
            Err(SourceError::NonDomain(cause)) => Err(TreestateReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(TreestateReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: GetTransaction,
{
    /// The raw transaction bytes plus where it lives, live from the validator.
    /// Passthrough: zaino does not re-serialize — a wallet parses the bytes
    /// locally. A missing txid is a domain miss (`Ok(None)`); a transport failure
    /// is transient.
    pub(crate) async fn raw_transaction(
        &self,
        id: TransactionId,
    ) -> Result<Option<RawTransaction>, TxReadError> {
        match self.source.get_transaction(id).await {
            Ok(TransactionResponse { bytes, location }) => Ok(Some(RawTransaction {
                data: bytes,
                location,
            })),
            Err(SourceError::Domain(GetTransactionError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(TxReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(TxReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: GetSubtreeRoots,
{
    /// Complete note-commitment subtree roots from `start_index`, live from the
    /// validator. Passthrough: the read is index-addressed exactly like the
    /// source. A pool that is not available is a definitive answer; a transport
    /// failure is transient.
    pub(crate) async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        match self
            .source
            .get_subtree_roots(pool, start_index, limit)
            .await
        {
            Ok(roots) => Ok(roots),
            Err(SourceError::Domain(GetSubtreeRootsError::PoolUnavailable(pool))) => Err(
                TreestateReadError::Fatal(format!("pool unavailable: {pool}")),
            ),
            Err(SourceError::NonDomain(cause)) => Err(TreestateReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(TreestateReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

// The three mempool reads bind separately (each to its own port) but share the
// single-source rule: a listing, its bytes, and its coherence tip must all come
// from the one source that serves the mempool — never a finalised secondary,
// which holds none. The routing that enforces that lives in the source adapter.
impl<Src> RemoteChainView<Src>
where
    Src: GetMempoolTxids,
{
    /// The txids currently in the validator's mempool, live. A validator that
    /// exposes no mempool is served as an *empty* mempool (an honest answer, not
    /// a failure); a transport failure is transient.
    pub(crate) async fn mempool_txids(&self) -> Result<Vec<TransactionId>, MempoolReadError> {
        match self.source.get_mempool_txids().await {
            Ok(txids) => Ok(txids),
            Err(SourceError::Domain(GetMempoolTxidsError::Unavailable)) => Ok(Vec::new()),
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: GetRawMempoolTransaction,
{
    /// The raw bytes of one mempool transaction, live. A txid the validator has
    /// since dropped — the listing/fetch race — is a domain miss (`Ok(None)`); a
    /// transport failure is transient.
    pub(crate) async fn raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Option<Vec<u8>>, MempoolReadError> {
        match self.source.get_raw_mempool_transaction(txid).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(SourceError::Domain(GetRawMempoolTransactionError::NotFound(_))) => Ok(None),
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: GetMempoolSourceTip,
{
    /// The chain tip the mempool listing is coherent against, live. The port
    /// carries no domain error (typed `Infallible`) — only a transport failure,
    /// reported transient.
    pub(crate) async fn mempool_source_tip(&self) -> Result<BlockId, MempoolReadError> {
        match self.source.get_mempool_source_tip().await {
            Ok((hash, height)) => Ok(BlockId { height, hash }),
            Err(SourceError::Domain(never)) => match never {},
            Err(SourceError::NonDomain(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
            Err(SourceError::Unavailable(cause)) => Err(MempoolReadError::Transient(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}
