//! The light-wallet routings.

use super::{Local, Passthrough, Routing, Withheld};

/// The lightwalletd-shaped routing with transparent address history served
/// **locally** from Zaino's own indexes, treestate relayed live to the
/// validator, and the node/explorer-only reads withheld.
///
/// Address history is [`Local`]: the light wallet's `GetTaddressBalance`,
/// `GetAddressUtxos` and `GetTaddressTxids` are answered from the finalised
/// store's transparent index and the non-finalised window — the store reports
/// its half whole, the window reports the receives it holds and which supplied
/// outpoints it saw spent, and the composer threads the two across the watermark.
/// Serving it locally means the wallet's queried addresses are never disclosed to
/// the validator, the privacy cost a local transparent index exists to remove.
///
/// Spend status stays [`Withheld`]: the light-wallet read-set never reads an
/// outpoint's spend state through the engine `Spend` placement — the window's
/// spend data reaches the address read through [`AddressReceiveRead`], not that
/// placement — so withholding it keeps the manifest honest, a capability no
/// served method consumes being `Absent`, not a false `Live`. Transaction
/// location is withheld for the same reason: no engine read dispatches on it.
///
/// Treestate stays [`Passthrough`]: the wallet witnesses against it, but no local
/// treestate index is built on any tier, so it is relayed to the validator.
///
/// [`AddressReceiveRead`]: zaino_service::AddressReceiveRead
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightWalletLocalRouting;

impl Routing for LightWalletLocalRouting {
    type Address = Local;
    type Treestate = Passthrough;
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
                Capability::Blocks | Capability::AddressHistory => {
                    assert_eq!(placement, PlacementKind::Local)
                }
                Capability::SpendStatus | Capability::TransactionLocation => {
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
