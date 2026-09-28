//! The light-wallet use case, served with everything the wallet parses itself
//! relayed to the validator.

use zaino_core::routing::LightWalletRouting;
use zaino_indexes::sets::compact_blocks::CompactBlocks;
use zaino_service::use_cases::LightWallet;
use zaino_source::{
    GetAddressBalance, GetAddressDeltas, GetAddressTxids, GetAddressUtxos,
    GetMempoolCompactTransaction, GetMempoolSourceTip, GetMempoolTxids, GetRawMempoolTransaction,
    GetSubtreeRoots, GetTransaction, GetTreestate, SendRawTransaction,
};

use crate::config::IndexedDeploymentConfig;
use crate::deployment::{Deployment, IndexedSource};
use crate::plan::RuntimePlan;
use crate::signals::ReadinessCriteria;

/// The light-wallet use case served with compact blocks composed locally from
/// the light-wallet index set and everything the wallet parses itself —
/// treestate, raw transactions, transparent history — relayed to the
/// validator; node reads withheld.
///
/// Address history is passthrough *for now*: it discloses queried addresses
/// to the validator, which a local transparent index exists to avoid. The
/// deployment that indexes it locally is a sibling of this one, not a change
/// to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LightWalletPassthrough;

impl Deployment for LightWalletPassthrough {
    type UseCase = LightWallet;
    type Routing = LightWalletRouting;
    type Indexes = CompactBlocks;
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
/// Safe to be wrong in one direction: a port missing here fails the demand
/// bound at the wiring, naming it.
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
    + GetAddressBalance
    + GetAddressUtxos
    + GetAddressTxids
    + GetAddressDeltas
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
        + GetAddressBalance
        + GetAddressUtxos
        + GetAddressTxids
        + GetAddressDeltas
{
}
