//! Capability: the outpoints currently unspent at a transparent
//! address.

use std::future::Future;
use std::ops::Range;

use zaino_primitives::types::{Height, TransparentAddress};

use crate::error::PortError;
use crate::outpoint::Outpoint;

/// Domain error for [`GetAddressUnspentOutpoints`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GetAddressUnspentOutpointsError {
    /// The address is not a well-formed transparent address.
    #[error("invalid transparent address: {0}")]
    InvalidAddress(TransparentAddress),
}

/// The transparent outputs unspent at an address as of the pinned tip,
/// restricted to outputs created within a half-open height range.
///
/// The range bounds the answer — an address of exchange scale can hold
/// millions of unspent outputs, and the range lets a driver walk them
/// in pieces instead of forcing one unbounded allocation. The answer
/// covers `range` clamped to the pinned view, ascending by creating
/// height; spentness itself is always judged as of the pinned tip. An
/// address with no unspent outputs there — including one the pinned
/// view has never seen — answers an empty list; only a malformed
/// address is a domain rejection. Zallet uses this for its default,
/// address-based spend detection.
pub trait GetAddressUnspentOutpoints: Send + Sync {
    /// The outpoints unspent at `address` whose creating transactions
    /// were mined within `range`.
    fn get_address_unspent_outpoints(
        &self,
        address: &TransparentAddress,
        range: Range<Height>,
    ) -> impl Future<Output = Result<Vec<Outpoint>, PortError<GetAddressUnspentOutpointsError>>> + Send;
}
