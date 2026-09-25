//! Which capabilities are answerable now, and a snapshot's serviceable range.
//!
//! Availability is layered. *Required* is what a use case demands (the profile
//! bounds). *Provided* is what a deployment's components can ever answer (the
//! composed type: an impl exists or it does not). *Serviceable* is what is
//! answerable right now, and to what height — this manifest. The manifest can
//! only narrow what the type already permits; it never widens it.

use zaino_primitives::types::Height;

/// The capability axis — one variant per capability trait, mirroring the index
/// set that backs it. Serviceability is "is this capability's index built?".
///
/// `EnumIter` gives compiler-generated exhaustive iteration (manifest
/// derivation) — a new variant is picked up automatically, no hand-kept list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, strum::EnumIter)]
pub enum Capability {
    Blocks,
    /// Where a transaction was mined — a local lookup (the `txid_location` index).
    TransactionLocation,
    /// A transaction's raw consensus bytes — served by the validator (no local index).
    RawTransaction,
    Treestate,
    AddressHistory,
    SpendStatus,
    SubtreeRoots,
    Mempool,
    Broadcast,
    ReportedUpgrades,
}

/// How far one capability can be answered right now.
///
/// Three questions, kept apart because collapsing them is the failure this
/// type prevents: a deployment never built to answer a capability looks the
/// same as one still syncing, and neither looks like one answering live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answerable {
    /// This deployment does not offer the capability: no component backs it and
    /// no provider is wired for it. Waiting will not help; configuration might.
    Absent,
    /// Offered, but nothing is serviceable yet — the backing index has no
    /// committed progress, or the provider is not reachable. Waiting will help.
    NotYet,
    /// Answerable for every height up to and including this one.
    ToHeight(Height),
    /// Answered live by a provider that is not height-bounded — the validator
    /// through a passthrough. Not pinned to any snapshot.
    Live,
}

/// For each capability, how far it is answerable right now.
///
/// Always has exactly one entry per [`Capability`]: built through
/// [`derive`](Self::derive), which iterates the enum, so a manifest cannot be
/// silent about a capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceabilityManifest {
    entries: Vec<(Capability, Answerable)>,
}

impl ServiceabilityManifest {
    /// Derive a manifest by asking `answer` about every capability.
    pub fn derive(answer: impl Fn(Capability) -> Answerable) -> Self {
        use strum::IntoEnumIterator;
        Self {
            entries: Capability::iter()
                .map(|capability| (capability, answer(capability)))
                .collect(),
        }
    }

    /// The same answer for every capability.
    pub fn uniform(answer: Answerable) -> Self {
        Self::derive(|_| answer)
    }

    /// How far `capability` is answerable.
    pub fn get(&self, capability: Capability) -> Answerable {
        self.entries
            .iter()
            .find(|(cap, _)| *cap == capability)
            .map(|(_, answer)| *answer)
            // `derive` covers every variant, so this arm is unreachable by
            // construction; `Absent` is the conservative answer regardless.
            .unwrap_or(Answerable::Absent)
    }

    /// Every capability with its answer.
    pub fn iter(&self) -> impl Iterator<Item = (Capability, Answerable)> + '_ {
        self.entries.iter().copied()
    }
}

/// The heights a snapshot can answer, and the FS/NFS boundary within them.
#[derive(Clone, Copy, Debug)]
pub struct ServiceableRange {
    /// Top of append-only finalised state.
    pub finalized_tip: Height,
    /// Pinned best-chain tip; `finalized_tip..=tip` is the non-finalised window.
    pub tip: Height,
}
