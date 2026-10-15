//! `getstandardfee`: the ZIP 317 marginal fee

use zcash_protocol::value::Zatoshis;

/// The standard fee from the validator's `getstandardfee` RPC
///
/// - ZIP 317 `marginal_fee`, zatoshis per logical action. ZIP 2009 lowers it from 5000 to 1000
/// - the validator's relay policy at the current height, forwarded verbatim
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardFee {
    pub fee_zat: Zatoshis,
    /// Fee-estimator version, `0` = the static ZIP 317 fee
    pub estimator_version: u32,
}
