//! `getstandardfee` — the fee wallets should pay per logical action.

use crate::types::Zatoshis;

/// The validator's recommended fee per logical action (ZIP 317) for a
/// transaction mined in the next block.
///
/// Policy, not consensus: the validator picks the value from its network and
/// next-block height, so Zaino passes it through rather than deriving one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardFee {
    /// Fee per logical action.
    pub fee_per_action: Zatoshis,

    /// Estimator version identifier; `0` is the only version defined so far.
    pub version: u32,
}
