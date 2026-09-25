//! Routing: which provider answers each capability, decided per use case, as
//! a type.
//!
//! A composed engine holds three providers — the finalised store, the
//! non-finalised head, and the validator through a passthrough — and every
//! capability could in principle be answered locally (composed from the two
//! chain tiers) or remotely (relayed live to the validator). Which one is a
//! **decision the use case makes**, not a property of the capability: a light
//! server passes transparent-address queries through today and accepts the
//! privacy cost; an explorer indexes them. The handler that serves the read
//! must not know which, and the composer must not decide on its own.
//!
//! So the decision is a type, [`Routing`], with one associated [`Placement`]
//! per capability whose placement varies. The composer implements each read
//! trait once, dispatching to a per-capability *placement trait* implemented
//! on the placement markers themselves — `Local` carries the bounds a local
//! merge needs of the chain tiers, `Remote` the source ports a passthrough
//! needs. Distinct `Self` types, so the impls cannot overlap; `Withheld`
//! implements none of them. A placement whose provider ports are missing is
//! an impl that does not exist — checked where the use case is wired, not
//! discovered per request.
//!
//! ```text
//! reads(Composed<Fs, Nfs, Src, R>) =
//!     { C : R::C = Local  ∧ Fs, Nfs provide C }
//!   ∪ { C : R::C = Remote ∧ Src provides C }
//! ```
//!
//! Capabilities whose placement does not vary are not on [`Routing`]: compact
//! blocks are always composed locally (that is what the tiers are for), and
//! raw transactions, broadcast, mempool and the upgrade schedule are always
//! the validator's (no local index exists for them).
//!
//! The same type drives the serviceability manifest: a withheld capability is
//! `Absent`, a remote one is `Live`, a local one reaches as far as its tiers
//! do. The manifest and the reads consult one declaration, so they cannot
//! disagree.

use zaino_core::Capability;

/// Where a capability is answered: from the local chain tiers, from the
/// validator, or nowhere (withheld by the deployment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementKind {
    /// Composed from the finalised store and the non-finalised head.
    Local,
    /// Relayed live to the validator through the passthrough provider.
    Remote,
    /// Not offered by this deployment, whatever its providers could answer.
    Withheld,
}

mod sealed {
    pub trait Sealed {}
}

/// A placement, as a type. Exactly three implementors: [`Local`], [`Remote`],
/// [`Withheld`].
pub trait Placement: sealed::Sealed + Send + Sync + 'static {
    /// The same placement, as a value — for the manifest derivation.
    const KIND: PlacementKind;
}

/// Answered from the local chain tiers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Local;
/// Answered live by the validator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Remote;
/// Not offered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Withheld;

impl sealed::Sealed for Local {}
impl sealed::Sealed for Remote {}
impl sealed::Sealed for Withheld {}

impl Placement for Local {
    const KIND: PlacementKind = PlacementKind::Local;
}
impl Placement for Remote {
    const KIND: PlacementKind = PlacementKind::Remote;
}
impl Placement for Withheld {
    const KIND: PlacementKind = PlacementKind::Withheld;
}

/// A use case's routing table, as a type.
///
/// One associated type per capability whose placement is a decision. Fixed
/// placements are not here; see [`placement`](Self::placement) for the full
/// map, which is exhaustive over [`Capability`] so a new variant must be
/// classified.
pub trait Routing: Send + Sync + 'static {
    /// Transparent address history: balance, UTXOs, deltas, txids.
    type Address: Placement;
    /// Commitment treestate and subtree roots.
    type Treestate: Placement;
    /// Whether and where an outpoint was spent.
    type Spend: Placement;
    /// Where a transaction was mined.
    type TransactionLocation: Placement;

    /// The placement of every capability under this routing.
    fn placement(capability: Capability) -> PlacementKind {
        match capability {
            // Always composed from the tiers — that is what they exist for.
            Capability::Blocks => PlacementKind::Local,
            Capability::AddressHistory => Self::Address::KIND,
            Capability::Treestate | Capability::SubtreeRoots => Self::Treestate::KIND,
            Capability::SpendStatus => Self::Spend::KIND,
            Capability::TransactionLocation => Self::TransactionLocation::KIND,
            // No local index exists for these; the validator is the only path.
            Capability::RawTransaction
            | Capability::Mempool
            | Capability::Broadcast
            | Capability::ReportedUpgrades => PlacementKind::Remote,
        }
    }
}

/// The lightwalletd-shaped routing: compact blocks local, everything the
/// wallet parses itself passed through, and the node/explorer reads withheld.
///
/// Address history is remote *for now* — it discloses queried addresses to
/// the validator, which a local transparent index exists to avoid. Flipping
/// it to [`Local`] is a one-line change here and a materialisation that
/// builds `address_history`; the compiler names anything else that is
/// missing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightRouting;

impl Routing for LightRouting {
    type Address = Remote;
    type Treestate = Remote;
    type Spend = Withheld;
    type TransactionLocation = Withheld;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_routing_places_every_capability() {
        use strum::IntoEnumIterator;
        for capability in Capability::iter() {
            // Exhaustiveness is rustc's; this pins the light table's shape.
            let placement = LightRouting::placement(capability);
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
                | Capability::ReportedUpgrades => {
                    assert_eq!(placement, PlacementKind::Remote)
                }
            }
        }
    }
}
