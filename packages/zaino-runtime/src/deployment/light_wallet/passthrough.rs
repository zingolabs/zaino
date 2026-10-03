//! The light-wallet use case, served with everything the wallet parses itself —
//! transparent address history included — relayed to the validator.

use zaino_core::routing::LightWalletPassthroughRouting;
use zaino_indexes::sets::compact_blocks::CompactBlocks;
use zaino_service::use_cases::LightWallet;
use zaino_source::{GetAddressBalance, GetAddressDeltas, GetAddressTxids, GetAddressUtxos};

use crate::config::IndexedDeploymentConfig;
use crate::deployment::Deployment;
use crate::plan::RuntimePlan;
use crate::signals::ReadinessCriteria;

/// The light-wallet use case served with compact blocks composed locally from
/// the compact-block index set and everything the wallet parses itself —
/// treestate, raw transactions, transparent address history — relayed to the
/// validator; node reads withheld.
///
/// Address history is passthrough: it discloses the wallet's queried addresses to
/// the validator, which a local transparent index exists to avoid. The sibling
/// [`LightWalletLocal`](super::LightWalletLocal) serves the same use case with
/// address history indexed locally; this deployment is for operators who accept
/// the disclosure in exchange for not building the transparent index set. It
/// therefore builds only [`CompactBlocks`] and requires the address source ports
/// (see [`LightWalletPassthroughSource`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightWalletPassthrough;

impl Deployment for LightWalletPassthrough {
    type UseCase = LightWallet;
    type Routing = LightWalletPassthroughRouting;
    type Indexes = CompactBlocks;
}

impl RuntimePlan for LightWalletPassthrough {
    type Config = IndexedDeploymentConfig;

    /// A local index is built, and the deployment is not serving until it has
    /// caught up: readiness gates on the indexer's sync.
    const READINESS: ReadinessCriteria = ReadinessCriteria { sync_gated: true };
}

/// What this deployment requires of the validator beyond the shared light-wallet
/// floor: the four transparent-address source ports its routing relays through.
///
/// Extends [`LightWalletSource`](super::LightWalletSource) — the indexed
/// assembly's floor plus the always-relayed wallet reads — with the address ports
/// `GetAddressBalance` / `GetAddressUtxos` / `GetAddressTxids` / `GetAddressDeltas`
/// that `Address = Passthrough` sends to the validator. The local deployment
/// drops exactly these. Safe to be wrong in one direction: a port missing here
/// fails the demand bound at the wiring, naming it — which the deployability
/// assertion below proves.
pub trait LightWalletPassthroughSource:
    super::LightWalletSource + GetAddressBalance + GetAddressUtxos + GetAddressTxids + GetAddressDeltas
{
}
impl<S> LightWalletPassthroughSource for S where
    S: super::LightWalletSource
        + GetAddressBalance
        + GetAddressUtxos
        + GetAddressTxids
        + GetAddressDeltas
{
}

/// The milestone, as a compile-time proof: any validator client meeting
/// [`LightWalletPassthroughSource`], composed with the **real** finalised store
/// and chain-head tiers under [`LightWalletPassthrough`], serves the light-wallet
/// use case — with address history relayed to the validator over the address
/// source ports.
///
/// Generic in the source, so the runtime's non-test code names no concrete
/// validator; the test below instantiates it with the production composite client
/// to pin the whole chain over the real store and head tiers.
#[cfg(test)]
fn light_wallet_serves<C: LightWalletPassthroughSource>() {
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
    /// that the production client is a
    /// [`LightWalletPassthroughSource`](super::LightWalletPassthroughSource) and
    /// that the engine it composes under
    /// [`LightWalletPassthrough`](super::LightWalletPassthrough) serves the
    /// light-wallet use case with address history relayed. A missing capability
    /// would fail here with the bound named; the fix belongs at the source, not
    /// behind a wider bound.
    #[test]
    fn the_production_engine_serves_light_wallet() {
        light_wallet_serves::<ValidatorClient<Arc<ZebraValidator>>>();
    }
}
