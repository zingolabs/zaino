//! Capability: the txids involving a transparent address over a
//! height range.

use std::future::Future;
use std::ops::Range;

use zaino_primitives::types::{Height, TransactionHash, TransparentAddress};

use crate::error::PortError;

/// Domain error for [`GetAddressTransactionIds`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GetAddressTransactionIdsError {
    /// The address is not a well-formed transparent address.
    #[error("invalid transparent address: {0}")]
    InvalidAddress(TransparentAddress),
}

/// The txids of transactions involving a transparent address, mined
/// within a half-open height range of the pinned view.
///
/// The answer covers `range` clamped to the pinned view, ascending by
/// mined height. An address with no history there answers an empty
/// list. Zallet uses this to recover a spending transaction once a
/// missed spend has been detected.
pub trait GetAddressTransactionIds: Send + Sync {
    /// The txids involving `address` mined within `range`.
    fn get_address_transaction_ids(
        &self,
        address: &TransparentAddress,
        range: Range<Height>,
    ) -> impl Future<Output = Result<Vec<TransactionHash>, PortError<GetAddressTransactionIdsError>>>
           + Send;
}
