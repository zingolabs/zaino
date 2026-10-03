//! The node-RPC / explorer use case, served with transparent address history and
//! spend lookups composed locally from Zaino's own indexes and every other node
//! and chain read the explorer parses itself relayed to the validator.

use zaino_core::routing::NodeRpcLocalRouting;
use zaino_indexes::sets::transparent_history::TransparentHistory;
use zaino_service::use_cases::NodeRpc;
use zaino_source::{
    GetBlock, GetBlockByHash, GetBlockDecoded, GetBlockDecodedByHash, GetBlockHeader,
    GetBlockVerbose, GetBlockVerboseByHash, GetBlockchainInfo, GetDifficulty, GetMempoolMetadata,
    GetMempoolSourceTip, GetMempoolTxids, GetMiningInfo, GetNetworkInfo, GetNetworkSolPs,
    GetNodeInfo, GetPeerInfo, GetRawBlock, GetRawBlockByHash, GetSubtreeRoots, GetTransaction,
    GetTransactionVerbose, GetTreestate, GetTxOut, Ping, SendRawTransaction,
};

use crate::config::IndexedDeploymentConfig;
use crate::deployment::{Deployment, IndexedSource};
use crate::plan::RuntimePlan;
use crate::signals::ReadinessCriteria;

/// The node-RPC / explorer use case served with compact blocks, transparent
/// address history and spend lookups composed locally from Zaino's own indexes,
/// and every other read the explorer parses itself — full and verbose blocks,
/// decoded transactions, the chain-info aggregate, treestate, raw transactions,
/// the node-status reads and the mempool listing — relayed to the validator;
/// transaction location withheld.
///
/// Address history is local because the explorer's address page needs
/// `getaddressdeltas` — full transparent history with receives and spends —
/// which no validator answers in plain RPC mode: Zebra has no such method. The
/// finalised store answers the whole address read over the [`TransparentHistory`]
/// index set, the non-finalised window reports its own receives and spends, and
/// the composer threads them across the seam. Serving it locally also means the
/// deployment relays no address queries to the validator, so it discloses
/// no queried addresses and demands no address source port (see
/// [`NodeRpcSource`]).
///
/// Spend lookup (`getspentinfo`) is local for the same reason: it is another
/// indexer-only method Zebra answers with `-32601`. It reads the spends index
/// [`TransparentHistory`] already builds — across the seam, so a spend in the
/// volatile window of an output created below the watermark is located — and so
/// demands no spend source port either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeRpcLocal;

impl Deployment for NodeRpcLocal {
    type UseCase = NodeRpc;
    type Routing = NodeRpcLocalRouting;
    type Indexes = TransparentHistory;
}

impl RuntimePlan for NodeRpcLocal {
    type Config = IndexedDeploymentConfig;

    /// A local index is built, and the deployment is not serving until it has
    /// caught up: readiness gates on the indexer's sync, as the light-wallet
    /// local deployment does.
    const READINESS: ReadinessCriteria = ReadinessCriteria { sync_gated: true };
}

/// What this deployment requires of the validator, as one name: the indexed
/// assembly's floor plus every port its routing's passthrough placements and
/// the always-passthrough reads relay through.
///
/// Hand-kept beside the deployment rather than derived, because Rust cannot
/// compute "the union of the source bounds of the impls this routing selects".
/// The set is derived from the `Src` bounds of the engine's node-RPC read and
/// control impls (block, verbose block, raw block, transaction, transaction
/// view, chain info, passthrough treestate, node status, mempool
/// listing/subscribe, broadcast) — not from memory. It names no address source
/// port: address history is served locally, so the engine's address read
/// dispatches to the store and head tiers, not the validator. That is what lets
/// the deployment run over a plain-RPC validator that cannot answer
/// `getaddressdeltas` at all. Safe to be wrong in one direction: a port missing
/// here fails the demand bound at the wiring, naming it, which is exactly what
/// the deployability assertion below proves.
pub trait NodeRpcSource:
    IndexedSource
    + GetBlock
    + GetBlockByHash
    + GetBlockHeader
    + GetBlockVerbose
    + GetBlockVerboseByHash
    + GetRawBlock
    + GetRawBlockByHash
    + GetTransactionVerbose
    + GetBlockDecoded
    + GetBlockDecodedByHash
    + GetTransaction
    + GetTreestate
    + GetSubtreeRoots
    + GetBlockchainInfo
    + GetNodeInfo
    + GetMiningInfo
    + GetPeerInfo
    + GetNetworkSolPs
    + GetDifficulty
    + GetNetworkInfo
    + Ping
    + GetTxOut
    + GetMempoolTxids
    + GetMempoolMetadata
    + GetMempoolSourceTip
    + SendRawTransaction
{
}
impl<S> NodeRpcSource for S where
    S: IndexedSource
        + GetBlock
        + GetBlockByHash
        + GetBlockHeader
        + GetBlockVerbose
        + GetBlockVerboseByHash
        + GetRawBlock
        + GetRawBlockByHash
        + GetTransactionVerbose
        + GetBlockDecoded
        + GetBlockDecodedByHash
        + GetTransaction
        + GetTreestate
        + GetSubtreeRoots
        + GetBlockchainInfo
        + GetNodeInfo
        + GetMiningInfo
        + GetPeerInfo
        + GetNetworkSolPs
        + GetDifficulty
        + GetNetworkInfo
        + Ping
        + GetTxOut
        + GetMempoolTxids
        + GetMempoolMetadata
        + GetMempoolSourceTip
        + SendRawTransaction
{
}

/// The milestone, as a compile-time proof: any validator client meeting
/// [`NodeRpcSource`], composed with the **real** finalised store and chain-head
/// tiers under [`NodeRpcLocal`], serves the node-RPC use case.
///
/// Generic in the source, so the runtime's non-test code names no concrete
/// validator (a `C: NodeRpcSource` that the engine cannot serve fails this body
/// to compile, with the missing bound named) — and the test below instantiates
/// it with the production composite client to pin the whole chain over the real
/// store and head tiers (`LmdbBackend`, `ChainHeadSubscriber` — the ones
/// [`IndexedEngine`](crate::IndexedEngine) wires).
#[cfg(test)]
fn node_rpc_serves<C: NodeRpcSource>() {
    fn serves<E: zaino_service::use_cases::Serves<NodeRpc>>() {}
    serves::<crate::IndexedEngine<NodeRpcLocal, C>>();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zaino_source::ValidatorClient;
    use zaino_source_zebra::ZebraValidator;

    use super::node_rpc_serves;

    /// The deployability milestone, over the real store and head tiers and the
    /// **production** validator client: the resilient composite over the shared
    /// `ZebraValidator` is exactly what `zainod` shares (its `client_over`
    /// returns `Arc<ValidatorClient<Arc<ZebraValidator>>>`). Compiling this
    /// instantiation proves both that the production client is a
    /// [`NodeRpcSource`](super::NodeRpcSource) and that the engine it composes
    /// under [`NodeRpcLocal`](super::NodeRpcLocal) serves the
    /// node-RPC use case. A missing capability would fail here with the bound
    /// named; the fix belongs at the source, not behind a wider bound.
    #[test]
    fn the_production_engine_serves_node_rpc() {
        node_rpc_serves::<ValidatorClient<Arc<ZebraValidator>>>();
    }
}
