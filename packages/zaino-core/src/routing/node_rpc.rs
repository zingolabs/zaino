//! The node-RPC / explorer routing.

use super::{Local, Routing, Withheld};

/// The node-RPC / explorer routing: compact blocks, transparent address history,
/// spend lookups and commitment treestate served **locally** from Zaino's own
/// indexes, transaction location withheld.
///
/// Address history is [`Local`] because the explorer's address page needs
/// `getaddressdeltas` — full transparent history, receives and spends — which no
/// validator answers in plain RPC mode: Zebra has no such method. The finalised
/// store answers the whole address read over its transparent index set, the
/// non-finalised window reports the receives it holds and which supplied
/// outpoints it saw spent, and the composer threads the two across the watermark
/// so a spend of an output received below it is attributed correctly.
///
/// Spend status is [`Local`] for the same reason: `getspentinfo` locates where an
/// outpoint was spent, which no validator answers in plain RPC mode (Zebra
/// returns `-32601`). Both tiers build the spends index, so the composer asks the
/// head first — a spend there is the newer fact — and falls through to the
/// finalised store, reporting a spend at or below the watermark of an output the
/// window never saw created.
///
/// Treestate is [`Local`]: the deployment builds the `tree_state` and per-pool
/// `subtrees_*` indexes, so `z_gettreestate` and `z_getsubtreesbyindex` are
/// answered from Zaino's own commitment-tree frontiers — the finalised store up
/// to the watermark, the window folding forward above it — rather than relayed to
/// the validator. The explorer's treestate surface is served from the same branch
/// as the compact blocks beside it.
///
/// Transaction location is withheld: no engine read dispatches on that placement,
/// so withholding it keeps the manifest honest — a capability no served method
/// consumes is `Absent`, not a false `Live`.
///
/// [`NodeRpcReads`]: zaino_service::read_sets::NodeRpcReads
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeRpcLocalRouting;

impl Routing for NodeRpcLocalRouting {
    type Address = Local;
    type Treestate = Local;
    type Spend = Local;
    type TransactionLocation = Withheld;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::PlacementKind;
    use zaino_service::Capability;

    #[test]
    fn node_rpc_routing_places_every_capability() {
        use strum::IntoEnumIterator;
        for capability in Capability::iter() {
            // Exhaustiveness is rustc's; this pins the node-RPC table's shape —
            // in particular that address history, spend status, treestate and
            // subtree roots are served locally, and that transaction location is
            // withheld, not silently passed through.
            let placement = NodeRpcLocalRouting::placement(capability);
            match capability {
                Capability::Blocks
                | Capability::AddressHistory
                | Capability::SpendStatus
                | Capability::Treestate
                | Capability::SubtreeRoots => {
                    assert_eq!(placement, PlacementKind::Local)
                }
                Capability::TransactionLocation => {
                    assert_eq!(placement, PlacementKind::Withheld)
                }
                Capability::RawTransaction
                | Capability::Mempool
                | Capability::Broadcast
                | Capability::NodeStatus
                | Capability::ReportedUpgrades => {
                    assert_eq!(placement, PlacementKind::Passthrough)
                }
            }
        }
    }
}
