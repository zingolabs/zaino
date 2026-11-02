//! Temporary owned → zebra translation.
//!
//! This module exists only while [`adopt_network`](super::adopt_network) must
//! return a `zebra_chain::parameters::Network`: the runtime consensus-parameters
//! type is still zebra's, consumed throughout `chain_index`, so the adopted
//! schedule — owned, keyed by consensus branch id — has to be spoken back in
//! zebra's `NetworkUpgrade` vocabulary to build and verify that `Network`.
//!
//! It is deliberately self-contained: the entire zebra surface that
//! `network_adoption` depends on is these three functions. When the runtime
//! network is domain-ized (a source-neutral consensus-parameters type),
//! nothing here is needed — delete the module and its `mod` declaration.
//!
//! TODO: remove once `chain_index` no longer consumes zebra's `Network` for
//! consensus parameters.

use zaino_primitives::types::{ConsensusBranchId, Height, NetworkUpgradeInfo};

/// The zebra `NetworkUpgrade` for an owned upgrade's branch id, or `None` when
/// this build does not know that branch — a validator ahead of Zaino, which
/// adoption skips rather than fails on. `branch_id` is the protocol identity;
/// zebra owns the branch-id → upgrade table, so the lookup lives here.
pub(super) fn network_upgrade(
    info: &NetworkUpgradeInfo,
) -> Option<zebra_chain::parameters::NetworkUpgrade> {
    zebra_chain::parameters::NetworkUpgrade::try_from(u32::from(info.branch_id)).ok()
}

/// Whether this build knows the upgrade a consensus branch id names.
///
/// Zero is the validator's "no branch id" marker for pre-Overwinter heights
/// (zebra's `RPC_MISSING_ID`), not an upgrade, so it counts as known.
pub(super) fn branch_known(branch: ConsensusBranchId) -> bool {
    let id = u32::from(branch);
    id == 0 || zebra_chain::parameters::NetworkUpgrade::try_from(id).is_ok()
}

/// An owned height as zebra's block height.
pub(super) fn height(height: Height) -> zebra_chain::block::Height {
    zebra_chain::block::Height(u32::from(height))
}
