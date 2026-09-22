//! `RemoteChainView` — the passthrough provider.
//!
//! One half of the local/passthrough split: the validator answered **live**,
//! through the `zaino-source` ports. It carries exactly the capabilities the
//! validator provides directly (broadcast today; treestate / transaction /
//! mempool / tip next), and never the ones zaino composes or indexes itself —
//! those belong to the local view (`ChainView`).
//!
//! It names only the source *ports*, never a concrete adapter: the composition
//! root injects the concrete `Src`. Which capabilities live here versus on the
//! local view is the classification — expressed as which provider carries the
//! trait, and checked where the two are composed (the engine and its coverage
//! assertion).
//!
//! Reads answered here are **live, not pinned** to a snapshot's tip — inherent
//! to passthrough (the validator's state cannot be pinned to our view). That is
//! sound for the immutable, historical data light clients query.

use core::fmt;

use zaino_core::TransactionId;
use zaino_service::error::BroadcastRejection;
use zaino_source::{OneShotSendRawTransaction, QueryError, SendRawTransactionError};

/// The passthrough provider over a source handle `Src`.
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
    /// Wrap a source handle as the passthrough provider.
    pub fn new(source: Src) -> Self {
        Self { source }
    }
}

impl<Src> RemoteChainView<Src>
where
    Src: OneShotSendRawTransaction<NonDomain: fmt::Display>,
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
            Err(QueryError::Domain(SendRawTransactionError::Malformed(reason))) => {
                Err(BroadcastRejection::Malformed(reason))
            }
            Err(QueryError::Domain(SendRawTransactionError::Rejected(reason))) => {
                Err(BroadcastRejection::Invalid(reason))
            }
            Err(QueryError::NonDomain(cause)) => Err(BroadcastRejection::Invalid(format!(
                "validator unavailable: {cause}"
            ))),
        }
    }
}
