//! The light-wallet use case, served with transparent address history composed
//! locally from Zaino's own indexes and everything else the wallet parses itself
//! relayed to the validator.

use zaino_core::routing::LightWalletLocalRouting;
use zaino_indexes::sets::transparent_history::TransparentHistory;
use zaino_service::use_cases::LightWallet;

use crate::config::IndexedDeploymentConfig;
use crate::deployment::Deployment;
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
/// the deployment does not relay address queries, it requires no address source
/// port — the shared [`LightWalletSource`](super::LightWalletSource) floor alone.
///
/// Growing the index set is a boot-visible change: a data directory already
/// synced under the compact-block set is refused by the index-coverage guard and
/// must be resynced from genesis into a fresh directory (see the runtime guide).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightWalletLocal;

impl Deployment for LightWalletLocal {
    type UseCase = LightWallet;
    type Routing = LightWalletLocalRouting;
    type Indexes = TransparentHistory;
}

impl RuntimePlan for LightWalletLocal {
    type Config = IndexedDeploymentConfig;

    /// A local index is built, and the deployment is not serving until it has
    /// caught up: readiness gates on the indexer's sync.
    const READINESS: ReadinessCriteria = ReadinessCriteria { sync_gated: true };
}

/// The milestone, as a compile-time proof: any validator client meeting the
/// shared [`LightWalletSource`](super::LightWalletSource) floor, composed with
/// the **real** finalised store and chain-head tiers under [`LightWalletLocal`],
/// serves the light-wallet use case — with address history composed locally over
/// the [`TransparentHistory`] index set, not relayed.
///
/// Generic in the source, so the runtime's non-test code names no concrete
/// validator; the test below instantiates it with the production composite client
/// to pin the whole chain over the real store and head tiers.
#[cfg(test)]
fn light_wallet_serves<C: super::LightWalletSource>() {
    fn serves<E: zaino_service::use_cases::Serves<LightWallet>>() {}
    serves::<crate::IndexedEngine<LightWalletLocal, C>>();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zaino_source::ValidatorClient;
    use zaino_source_zebra::ZebraValidator;

    use super::light_wallet_serves;

    /// The deployability milestone, over the real store and head tiers and the
    /// **production** validator client: compiling this instantiation proves both
    /// that the production client meets the shared
    /// [`LightWalletSource`](super::super::LightWalletSource) floor and that the
    /// engine it composes under [`LightWalletLocal`](super::LightWalletLocal)
    /// serves the light-wallet use case with address history local. A missing
    /// capability would fail here with the bound named; the fix belongs at the
    /// source, not behind a wider bound.
    #[test]
    fn the_production_engine_serves_light_wallet() {
        light_wallet_serves::<ValidatorClient<Arc<ZebraValidator>>>();
    }
}
