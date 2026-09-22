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

use zaino_core::{Height, TransactionId, Treestate};
use zaino_service::error::{BroadcastRejection, TreestateReadError};
use zaino_source::{
    GetTreestate, GetTreestateError, SendRawTransaction, SendRawTransactionError, SourceError,
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
