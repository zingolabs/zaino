//! The light-wallet use case, served with transparent address history composed
//! locally from Zaino's own indexes and everything else the wallet parses itself
//! relayed to the validator.

use zaino_core::routing::LightWalletRouting;
use zaino_indexes::sets::transparent_history::TransparentHistory;
use zaino_service::use_cases::LightWallet;
use zaino_source::{
    GetMempoolCompactTransaction, GetMempoolSourceTip, GetMempoolTxids, GetRawMempoolTransaction,
    GetSubtreeRoots, GetTransaction, GetTreestate, SendRawTransaction,
};

use crate::config::IndexedDeploymentConfig;
use crate::deployment::{Deployment, IndexedSource};
use crate::plan::RuntimePlan;
use crate::signals::ReadinessCriteria;

/// The light-wallet use case served with compact blocks and transparent address
/// history composed locally from Zaino's own indexes, and everything else the
/// wallet parses itself — treestate, raw transactions — relayed to the
/// validator; node reads withheld.
///
/// Address history is local because serving it from a transparent index keeps the
/// wallet's queried addresses off the validator, the privacy cost relaying them
/// imposes. The finalised store answers the whole address read over the
/// [`TransparentHistory`] index set, the non-finalised window reports its own
/// receives and spends, and the composer threads them across the seam. Because
/// the deployment no longer relays address queries, it demands no address source
/// port (see [`LightWalletSource`]).
///
/// The marker keeps the name `LightWalletPassthrough` although address history is
/// now local; renaming is deferred because the name is the kebab-case deployment
/// value in the live `zainod` config.
///
/// Growing the index set is a boot-visible change: a data directory already
/// synced under the compact-block set is refused by the index-coverage guard and
/// must be resynced from genesis into a fresh directory (see the runtime guide).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightWalletPassthrough;

impl Deployment for LightWalletPassthrough {
    type UseCase = LightWallet;
    type Routing = LightWalletRouting;
    type Indexes = TransparentHistory;
}

impl RuntimePlan for LightWalletPassthrough {
    type Config = IndexedDeploymentConfig;

    /// A local index is built, and the deployment is not serving until it has
    /// caught up: readiness gates on the indexer's sync.
    const READINESS: ReadinessCriteria = ReadinessCriteria { sync_gated: true };
}

/// What this deployment requires of the validator, as one name: the indexed
/// assembly's floor plus every port its routing's passthrough placements and
/// the always-passthrough reads relay through.
///
/// Hand-kept beside the deployment rather than derived, because Rust cannot
/// compute "the union of the source bounds of the impls this routing selects".
/// It names no address source port: address history is served locally, so the
/// engine's address read dispatches to the store and head tiers, not the
/// validator. Safe to be wrong in one direction: a port missing here fails the
/// demand bound at the wiring, naming it — which is what the deployability
/// assertion below proves.
pub trait LightWalletSource:
    IndexedSource
    + SendRawTransaction
    + GetMempoolTxids
    + GetMempoolSourceTip
    + GetRawMempoolTransaction
    + GetMempoolCompactTransaction
    + GetTransaction
    + GetTreestate
    + GetSubtreeRoots
{
}
impl<S> LightWalletSource for S where
    S: IndexedSource
        + SendRawTransaction
        + GetMempoolTxids
        + GetMempoolSourceTip
        + GetRawMempoolTransaction
        + GetMempoolCompactTransaction
        + GetTransaction
        + GetTreestate
        + GetSubtreeRoots
{
}

/// The milestone, as a compile-time proof: any validator client meeting
/// [`LightWalletSource`], composed with the **real** finalised store and
/// chain-head tiers under [`LightWalletPassthrough`], serves the light-wallet use
/// case — now with address history composed locally over the
/// [`TransparentHistory`] index set, not relayed.
///
/// Generic in the source, so the runtime's non-test code names no concrete
/// validator; the test below instantiates it with the production composite client
/// to pin the whole chain over the real store and head tiers.
#[cfg(test)]
fn light_wallet_serves<C: LightWalletSource>() {
    fn serves<E: zaino_service::use_cases::Serves<LightWallet>>() {}
    serves::<crate::IndexedEngine<LightWalletPassthrough, C>>();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zaino_source::ValidatorClient;
    use zaino_source_zebra::ZebraValidator;

    use super::light_wallet_serves;

    /// The deployability milestone, over the real store and head tiers and the
    /// **production** validator client: compiling this instantiation proves both
    /// that the production client is a [`LightWalletSource`](super::LightWalletSource)
    /// and that the engine it composes under
    /// [`LightWalletPassthrough`](super::LightWalletPassthrough) serves the
    /// light-wallet use case with address history local. A missing capability would
    /// fail here with the bound named; the fix belongs at the source, not behind a
    /// wider bound.
    #[test]
    fn the_production_engine_serves_light_wallet() {
        light_wallet_serves::<ValidatorClient<Arc<ZebraValidator>>>();
    }
}
