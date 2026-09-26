//! What a chain view can answer, and how far.

use zaino_primitives::types::Height;

/// One thing a chain view may be able to answer.
///
/// Domain-shaped: it names a question a consumer asks, not an index some
/// provider happens to keep. Deliberately not
/// [`zaino_chain_store::StoreCapability`], which names an index a database
/// holds — the store's set is an *input* here, and the mapping is not
/// one-to-one (`SpentOutputs` backs both spend status and address history;
/// `Core` has no consumer-facing analogue).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ChainCapability {
    /// Blocks and headers, by height or hash.
    Blocks,
    /// Compact blocks, for wallet sync.
    CompactBlocks,
    /// Transactions, and where they were mined.
    Transactions,
    /// Commitment tree state.
    Treestate,
    /// Subtree roots for a shielded pool.
    SubtreeRoots,
    /// The tips of the retained graph, and fork reconciliation.
    ChainTips,
    /// Transparent address history.
    AddressHistory,
    /// Whether an outpoint has been spent, and by what.
    SpendStatus,
    /// The unspent transparent output set's running totals.
    TxOutSet,
}

impl ChainCapability {
    /// Every capability, ascending.
    ///
    /// Enumerating this is what makes the manifest a derivation rather than a
    /// hand-kept list: a capability added here appears without anyone
    /// remembering to add it, which is what stops "what we advertise" and
    /// "what we answer" drifting apart.
    pub const ALL: [Self; 9] = [
        Self::Blocks,
        Self::CompactBlocks,
        Self::Transactions,
        Self::Treestate,
        Self::SubtreeRoots,
        Self::ChainTips,
        Self::AddressHistory,
        Self::SpendStatus,
        Self::TxOutSet,
    ];
}

/// The capabilities a deployment has chosen to offer.
///
/// A set over [`ChainCapability`], and the one place "what we advertise" and
/// "what we answer" are decided together: the manifest reports a capability
/// outside it as [`Answerable::Absent`], and a read for one is refused as not
/// serviceable. Two lists would be two things to keep in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedCapabilities(u16);

impl ServedCapabilities {
    /// Everything. The default, and what
    /// [`ChainViewComposer::new`](crate::ChainViewComposer::new) uses.
    ///
    /// Safe as a default precisely because it is not a claim: a capability the
    /// tiers cannot answer is still reported `Absent`, because the manifest
    /// consults coverage and the store's index set as well as this.
    pub const ALL: Self = Self(u16::MAX);

    /// Only the capabilities every chain view answers.
    pub const CORE: Self = Self(
        Self::bit(ChainCapability::Blocks)
            | Self::bit(ChainCapability::CompactBlocks)
            | Self::bit(ChainCapability::Transactions)
            | Self::bit(ChainCapability::Treestate)
            | Self::bit(ChainCapability::SubtreeRoots)
            | Self::bit(ChainCapability::ChainTips),
    );

    /// Whether this deployment offers `capability`.
    pub fn contains(self, capability: ChainCapability) -> bool {
        self.0 & Self::bit(capability) != 0
    }

    /// The same set, additionally offering `capability`.
    pub(crate) fn with(self, capability: ChainCapability) -> Self {
        Self(self.0 | Self::bit(capability))
    }

    /// One capability's bit.
    const fn bit(capability: ChainCapability) -> u16 {
        1 << (capability as u16)
    }
}

impl Default for ServedCapabilities {
    fn default() -> Self {
        Self::ALL
    }
}

impl core::fmt::Display for ChainCapability {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Blocks => "blocks",
            Self::CompactBlocks => "compact blocks",
            Self::Transactions => "transactions",
            Self::Treestate => "treestate",
            Self::SubtreeRoots => "subtree roots",
            Self::ChainTips => "chain tips",
            Self::AddressHistory => "transparent address history",
            Self::SpendStatus => "spend status",
            Self::TxOutSet => "the txout set",
        })
    }
}

/// How far a chain view can answer one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answerable {
    /// Not implemented by this composition, or not opted into.
    Absent,
    /// Implemented, but nothing is serviceable at the moment.
    NotAnswerable,
    /// Answerable for every height up to and including this one.
    ToHeight(Height),
}

/// The heights a view can answer between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceableRange {
    /// The top of the finalised store's coverage, or `None` when it holds
    /// nothing or is disabled.
    pub finalised_tip: Option<Height>,
    /// The pinned best-chain tip.
    pub tip: Option<Height>,
    /// The lowest height above the store that no provider *locally* covers.
    pub gap_from: Option<Height>,
}

/// What a chain view currently offers, and to what height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceabilityManifest {
    entries: [(ChainCapability, Answerable); ChainCapability::ALL.len()],
}

impl ServiceabilityManifest {
    /// Builds a manifest by asking `answerable` about each capability.
    pub fn derive(mut answerable: impl FnMut(ChainCapability) -> Answerable) -> Self {
        Self {
            entries: ChainCapability::ALL.map(|capability| (capability, answerable(capability))),
        }
    }

    /// How far this view can answer `capability`.
    pub fn get(&self, capability: ChainCapability) -> Answerable {
        self.entries
            .iter()
            .find(|(candidate, _)| *candidate == capability)
            .map(|(_, answerable)| *answerable)
            // Unreachable while `entries` is built from `ALL`, which `derive`
            // is the only way to do. Answering `Absent` rather than panicking
            // keeps a serviceability query — the call a consumer makes to find
            // out what is safe to ask — from being the one that ends the
            // process.
            .unwrap_or(Answerable::Absent)
    }

    /// Every capability and how far it is answerable, ascending.
    pub fn iter(&self) -> impl Iterator<Item = (ChainCapability, Answerable)> + '_ {
        self.entries.iter().copied()
    }

    /// Every capability answerable to at least `height`.
    pub fn answerable_at(&self, height: Height) -> impl Iterator<Item = ChainCapability> + '_ {
        self.entries
            .iter()
            .filter(move |(_, answerable)| match answerable {
                Answerable::ToHeight(top) => *top >= height,
                Answerable::Absent | Answerable::NotAnswerable => false,
            })
            .map(|(capability, _)| *capability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("test height is within the protocol limit")
    }

    #[test]
    fn all_contains_every_capability() {
        for capability in ChainCapability::ALL {
            match capability {
                ChainCapability::Blocks
                | ChainCapability::CompactBlocks
                | ChainCapability::Transactions
                | ChainCapability::Treestate
                | ChainCapability::SubtreeRoots
                | ChainCapability::ChainTips
                | ChainCapability::AddressHistory
                | ChainCapability::SpendStatus
                | ChainCapability::TxOutSet => {}
            }
        }
        let mut seen = ChainCapability::ALL;
        seen.sort();
        let mut unique = seen.to_vec();
        unique.dedup();
        assert_eq!(unique.len(), ChainCapability::ALL.len(), "duplicate in ALL");
    }

    /// A derived manifest answers for every capability, not only the ones the
    /// closure had an opinion about.
    #[test]
    fn a_derived_manifest_covers_every_capability() {
        let manifest = ServiceabilityManifest::derive(|capability| match capability {
            ChainCapability::Blocks => Answerable::ToHeight(height(100)),
            _ => Answerable::Absent,
        });

        assert_eq!(manifest.iter().count(), ChainCapability::ALL.len());
        assert_eq!(
            manifest.get(ChainCapability::Blocks),
            Answerable::ToHeight(height(100))
        );
        assert_eq!(manifest.get(ChainCapability::TxOutSet), Answerable::Absent);
    }

    /// `answerable_at` includes a capability reaching exactly the height asked
    /// for, and excludes one stopping just below it.
    #[test]
    fn answerable_at_is_inclusive_of_its_boundary() {
        let manifest = ServiceabilityManifest::derive(|capability| match capability {
            ChainCapability::Blocks => Answerable::ToHeight(height(100)),
            ChainCapability::CompactBlocks => Answerable::ToHeight(height(99)),
            _ => Answerable::Absent,
        });

        assert_eq!(
            manifest.answerable_at(height(100)).collect::<Vec<_>>(),
            vec![ChainCapability::Blocks]
        );
    }
}
