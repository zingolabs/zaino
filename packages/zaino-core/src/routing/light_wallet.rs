//! The light-wallet routings.
//!
//! Two, differing only in where transparent address history is answered:
//! [`LightWalletPassthroughRouting`] relays it to the validator,
//! [`LightWalletLocalRouting`] composes it from Zaino's own indexes. Everything
//! else is identical.

use super::{Local, Passthrough, Routing, Withheld};

/// The lightwalletd-shaped routing with transparent address history **relayed**
/// to the validator, treestate relayed too, and the node/explorer-only reads
/// withheld.
///
/// Address history is [`Passthrough`]: the wallet's `GetTaddressBalance`,
/// `GetAddressUtxos` and `GetTaddressTxids` are relayed to the validator, which
/// discloses the queried addresses to it — the privacy cost a local transparent
/// index exists to remove. The deployment under this routing builds only the
/// compact-block index set and requires the address source ports;
/// [`LightWalletLocalRouting`] is the sibling that indexes the history instead.
///
/// Spend status and transaction location stay [`Withheld`] and treestate stays
/// [`Passthrough`], exactly as under the local routing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightWalletPassthroughRouting;

impl Routing for LightWalletPassthroughRouting {
    type Address = Passthrough;
    type Treestate = Passthrough;
    type Spend = Withheld;
    type TransactionLocation = Withheld;
}

/// The lightwalletd-shaped routing with transparent address history and
/// treestate served **locally** from Zaino's own indexes, and the
/// node/explorer-only reads withheld.
///
/// Address history is [`Local`]: the light wallet's `GetTaddressBalance`,
/// `GetAddressUtxos` and `GetTaddressTxids` are answered from the finalised
/// store's transparent index and the non-finalised window — the store reports
/// its half whole, the window reports the receives it holds and which supplied
/// outpoints it saw spent, and the composer threads the two across the watermark.
/// Serving it locally means the wallet's queried addresses are never disclosed to
/// the validator, the privacy cost a local transparent index exists to remove.
///
/// Treestate is [`Local`]: the deployment builds the `tree_state` and per-pool
/// `subtrees_*` indexes, so `GetTreeState` / `GetLatestTreeState` and
/// `GetSubtreeRoots` are answered from Zaino's own commitment-tree frontiers —
/// the finalised store up to the watermark, the window folding forward above it —
/// rather than a validator round trip on every scan batch. This is the whole
/// point of the local light-wallet deployment: the treestate a wallet witnesses
/// against is served from the same branch as the compact blocks beside it.
///
/// Spend status stays [`Withheld`]: the light-wallet read-set never reads an
/// outpoint's spend state through the engine `Spend` placement — the window's
/// spend data reaches the address read through [`AddressReceiveRead`], not that
/// placement — so withholding it keeps the manifest honest, a capability no
/// served method consumes being `Absent`, not a false `Live`. Transaction
/// location is withheld for the same reason: no engine read dispatches on it.
///
/// [`AddressReceiveRead`]: zaino_service::AddressReceiveRead
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightWalletLocalRouting;

impl Routing for LightWalletLocalRouting {
    type Address = Local;
    type Treestate = Local;
    type Spend = Withheld;
    type TransactionLocation = Withheld;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::PlacementKind;
    use zaino_service::Capability;

    #[test]
    fn light_local_routing_places_every_capability() {
        use strum::IntoEnumIterator;
        for capability in Capability::iter() {
            // Exhaustiveness is rustc's; this pins the local light table's shape —
            // in particular that address history is served locally, and that spend
            // status and transaction location are withheld, not passed through.
            let placement = LightWalletLocalRouting::placement(capability);
            match capability {
                // Address history, treestate and subtree roots are all composed
                // from Zaino's own indexes under this routing.
                Capability::Blocks
                | Capability::AddressHistory
                | Capability::Treestate
                | Capability::SubtreeRoots => {
                    assert_eq!(placement, PlacementKind::Local)
                }
                Capability::SpendStatus | Capability::TransactionLocation => {
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

    #[test]
    fn light_passthrough_routing_places_every_capability() {
        use strum::IntoEnumIterator;
        for capability in Capability::iter() {
            // The passthrough table differs from the local one in exactly one
            // place: address history is relayed to the validator, not composed
            // locally. Spend status and transaction location stay withheld.
            let placement = LightWalletPassthroughRouting::placement(capability);
            match capability {
                Capability::Blocks => assert_eq!(placement, PlacementKind::Local),
                Capability::SpendStatus | Capability::TransactionLocation => {
                    assert_eq!(placement, PlacementKind::Withheld)
                }
                Capability::AddressHistory
                | Capability::Treestate
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
