//! Routing: which provider answers each capability, decided per deployment,
//! as a type.
//!
//! A composed engine holds three providers — the finalised store, the
//! non-finalised head, and the validator through a passthrough — and every
//! capability could in principle be answered locally (composed from the two
//! chain tiers) or by passthrough (relayed live to the validator). Which one is a
//! **decision the deployment makes**, not a property of the capability: a light
//! server passes transparent-address queries through today and accepts the
//! privacy cost; an explorer indexes them. The handler that serves the read
//! must not know which, and the composer must not decide on its own.
//!
//! So the decision is a type, [`Routing`], with one associated [`Placement`]
//! per capability whose placement varies. The composer implements each read
//! trait once, dispatching to a per-capability *placement trait* implemented
//! on the placement markers themselves — [`Local`] carries the bounds a local
//! merge needs of the chain tiers, [`Passthrough`] the source ports a passthrough
//! needs. Distinct `Self` types, so the impls cannot overlap; [`Withheld`]
//! implements none of them. A placement whose provider ports are missing is
//! an impl that does not exist — checked where the use case is wired, not
//! discovered per request.
//!
//! ```text
//! reads(Engine<Fs, Nfs, Src, R>) =
//!     { C : R::C = Local  ∧ Fs, Nfs provide C }
//!   ∪ { C : R::C = Passthrough ∧ Src provides C }
//! ```
//!
//! Capabilities whose placement does not vary are not on [`Routing`]: compact
//! blocks are always composed locally (that is what the tiers are for), and
//! raw transactions, broadcast, mempool and the upgrade schedule are always
//! the validator's (no local index exists for them).
//!
//! The same type drives the serviceability manifest: a withheld capability is
//! `Absent`, a passthrough one is `Live`, a local one reaches as far as its tiers
//! do. The manifest and the reads consult one declaration, so they cannot
//! disagree.
//!
//! The placement markers ([`Local`], [`Passthrough`], [`Withheld`]) live in the
//! `placement` submodule; the concrete routings in the `light_wallet` submodule
//! (one per address placement) and the `node_rpc` one.

mod light_wallet;
mod node_rpc;
mod placement;

pub use light_wallet::{LightWalletLocalRouting, LightWalletPassthroughRouting};
pub use node_rpc::NodeRpcLocalRouting;
pub use placement::{Local, Passthrough, Placement, Withheld};

use zaino_service::Capability;

/// Where a capability is answered: from the local chain tiers, from the
/// validator, or nowhere (withheld by the deployment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementKind {
    /// Composed from the finalised store and the non-finalised head.
    Local,
    /// Relayed live to the validator through the passthrough provider.
    Passthrough,
    /// Not offered by this deployment, whatever its providers could answer.
    Withheld,
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
            | Capability::NodeStatus
            | Capability::ReportedUpgrades => PlacementKind::Passthrough,
        }
    }
}
