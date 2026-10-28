//! `getblocksubsidy` — how a block's subsidy is split at a given height.

use crate::types::Zatoshis;

/// The block subsidy split at a given height.
///
/// Amounts are zatoshis throughout. The wire form reports them in ZEC, as
/// either a JSON number or a string depending on validator; normalising that to
/// integer zatoshis is the adapter's job, so no float ever reaches a consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSubsidy {
    /// The miner's share.
    pub miner: Zatoshis,

    /// The founders' reward.
    ///
    /// Zero after the founders' reward period ends; the field is still reported.
    pub founders: Zatoshis,

    /// Total across all direct funding streams.
    pub funding_streams_total: Zatoshis,

    /// Total sent to development funding lockboxes.
    pub lockbox_total: Zatoshis,

    /// The block subsidy as a whole.
    pub total_block_subsidy: Zatoshis,

    /// Per-recipient breakdown of the direct funding streams.
    ///
    /// Empty when no funding streams are active at this height — an empty list
    /// says exactly that, so this is not an `Option`.
    pub funding_streams: Vec<FundingStream>,

    /// Per-recipient breakdown of the development fund lockbox streams.
    ///
    /// Empty when no lockbox streams are active at this height.
    pub lockbox_streams: Vec<LockboxStream>,
}

/// One direct funding stream within a block subsidy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingStream {
    /// Description of the recipient.
    pub recipient: String,
    /// URL of the specification defining this stream.
    pub specification: String,
    /// The stream's share of the subsidy.
    pub value: Zatoshis,
    /// Recipient address.
    ///
    /// `None` for streams paid to a lockbox rather than an address.
    pub address: Option<String>,
}

/// One development fund lockbox stream within a block subsidy.
///
/// Distinct from [`FundingStream`] because a lockbox accrues value rather than
/// paying it to an address, so it has no recipient address at all — the absence
/// is structural, not a missing field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockboxStream {
    /// Description of the lockbox.
    pub recipient: String,
    /// URL of the specification defining this lockbox.
    pub specification: String,
    /// The amount locked.
    pub value: Zatoshis,
}
