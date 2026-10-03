//! The node-RPC / explorer routing.

use super::{Local, Passthrough, Routing, Withheld};

/// The node-RPC / explorer routing: compact blocks, transparent address history
/// and spend lookups served **locally** from Zaino's own indexes, treestate
/// relayed live to the validator, transaction location withheld.
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
/// Transaction location is withheld: no engine read dispatches on that placement,
/// so withholding it keeps the manifest honest — a capability no served method
/// consumes is `Absent`, not a false `Live`.
///
/// Treestate stays [`Passthrough`]: the explorer surface reads it, but no local
/// treestate index is built on any tier, so it is relayed to the validator.
///
/// [`NodeRpcReads`]: zaino_service::read_sets::NodeRpcReads
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeRpcLocalRouting;

impl Routing for NodeRpcLocalRouting {
    type Address = Local;
    type Treestate = Passthrough;
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
            // in particular that address history and spend status are served
            // locally, and that transaction location is withheld, not silently
            // passed through.
            let placement = NodeRpcLocalRouting::placement(capability);
            match capability {
                Capability::Blocks | Capability::AddressHistory | Capability::SpendStatus => {
                    assert_eq!(placement, PlacementKind::Local)
                }
                Capability::TransactionLocation => {
                    assert_eq!(placement, PlacementKind::Withheld)
                }
                Capability::Treestate
                | Capability::SubtreeRoots
                | Capability::RawTransaction
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
