//! `getaddressdeltas` — transparent balance changes over a height range.
//!
//! zcashd's method, not Zebra's. The request has two shapes and the response
//! shape depends on which was asked, so both are modelled here rather than
//! being flattened into one type with optional fields.

use crate::types::{AddressDelta, BlockRef, TransparentAddress};

/// What was asked of `getaddressdeltas`.
///
/// # Height range
///
/// The interface's range is open-ended: zcashd treats `0` as "unbounded" in
/// either position, and the single-address form carries no range at all. Both
/// arrive here unresolved. Clamping them against the chain tip is the answering
/// adapter's job, because only it knows the tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressDeltasRequest {
    /// Deltas for one address across the whole chain.
    ///
    /// Answered with [`AddressDeltas::Simple`].
    Address(TransparentAddress),

    /// Deltas for several addresses over a height range.
    Filtered {
        /// Addresses to report on.
        addresses: Vec<TransparentAddress>,
        /// First height to include. `0` means "from the genesis block".
        start: u32,
        /// Last height to include. `0` means "to the chain tip".
        end: u32,
        /// Whether the caller asked for the range's endpoints to be echoed
        /// back, which selects [`AddressDeltas::WithChainInfo`].
        chain_info: bool,
    },
}

/// The answer to a [`AddressDeltasRequest`].
///
/// Which variant is correct is decided by the request, not by what the data
/// turned out to be: a `chain_info` request with no deltas still answers
/// [`Self::WithChainInfo`] with an empty list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressDeltas {
    /// Just the deltas.
    Simple(Vec<AddressDelta>),

    /// The deltas plus the resolved endpoints of the range they cover.
    WithChainInfo {
        /// The deltas.
        deltas: Vec<AddressDelta>,
        /// The first block of the range, after clamping.
        start: BlockRef,
        /// The last block of the range, after clamping.
        end: BlockRef,
    },
}
