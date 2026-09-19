//! Read-composition **policy** — the pure decision half.
//!
//! For each capability, how a request is answered from the components: **route**
//! (one tier by height), **merge** (both tiers), or **passthrough** (the
//! validator). One policy, in one place, so no consumer re-derives it (locality
//! of correctness). This is the [`Capability`] ⇄ provision relation as code —
//! notably, the single site that declares which capabilities have **no local
//! index** and are served by the validator.
//!
//! This module is the *decision* only — pure over `Capability` + `Height`, so it
//! needs neither the churning primitives nor a live provider. **Execution** —
//! actually routing a `Route`/`Merge` read to the finalised store / chain head,
//! or a `Passthrough` read to the validator, and merging the results — is the
//! read-composition layer, which lands with the reader (Phase 3/4). Keeping the
//! decision here, testable now, is deliberate: the strategy map is stable policy;
//! only its wiring waits.

use zaino_core::{Capability, Height};

/// Which local tier owns a routed read: the finalised store, or the recent
/// (non-finalised) chain head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The finalised store — heights at or below the watermark.
    Finalised,
    /// The recent chain head — heights above the watermark.
    Recent,
}

/// How a capability is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// One tier, chosen by height at the watermark (compact block, block-by-hash).
    Route,
    /// Both tiers combined (transparent address history, spend status): the
    /// finalised set with recent spends removed, plus recent creates.
    Merge,
    /// No local index — the validator answers, keyed by immutable id. Full/raw
    /// block and raw tx, the node-operator RPCs, and (as verified against
    /// `zaino-state`) treestate and subtree roots, which are not indexed
    /// locally.
    Passthrough,
}

/// The static serving strategy for each capability — the capability ⇄ provision
/// map. Exhaustive by design: a new [`Capability`] variant must be classified.
pub fn strategy(cap: Capability) -> Strategy {
    match cap {
        // Locally indexed, one tier by height.
        Capability::Blocks => Strategy::Route,
        // Locally indexed, combined across the finalised/recent boundary. A
        // transaction's *location* (which block mined it) is a local lookup that
        // may land either side of the boundary; its raw bytes are separate
        // (`RawTransaction`, passthrough).
        Capability::AddressHistory | Capability::SpendStatus | Capability::TransactionLocation => {
            Strategy::Merge
        }
        // Not indexed locally — the validator is the only path. Treestate and
        // subtree roots need the commitment-tree frontier, which zaino-state
        // does not build; it forwards to `z_gettreestate` / subtree RPCs. Raw
        // transaction bytes are likewise fetched from the validator.
        Capability::RawTransaction
        | Capability::Treestate
        | Capability::SubtreeRoots
        | Capability::Mempool
        | Capability::Broadcast
        | Capability::ReportedUpgrades => Strategy::Passthrough,
    }
}

/// The `route(h)` domain split: the finalised tier owns `h <= watermark`, the
/// recent tier owns `h > watermark`. Pure boundary; the recent-not-ready case is
/// the snapshot's concern.
pub fn tier_of(height: Height, watermark: Height) -> Tier {
    if height <= watermark {
        Tier::Finalised
    } else {
        Tier::Recent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("valid height")
    }

    #[test]
    fn compact_blocks_route_by_height() {
        assert_eq!(strategy(Capability::Blocks), Strategy::Route);
    }

    #[test]
    fn address_and_spend_merge_across_the_boundary() {
        assert_eq!(strategy(Capability::AddressHistory), Strategy::Merge);
        assert_eq!(strategy(Capability::SpendStatus), Strategy::Merge);
    }

    #[test]
    fn treestate_and_subtree_roots_are_passthrough() {
        // Corrected: zaino-state serves both from the validator, not a local
        // commitment-tree index.
        assert_eq!(strategy(Capability::Treestate), Strategy::Passthrough);
        assert_eq!(strategy(Capability::SubtreeRoots), Strategy::Passthrough);
    }

    #[test]
    fn node_and_control_capabilities_are_passthrough() {
        assert_eq!(strategy(Capability::RawTransaction), Strategy::Passthrough);
        // A transaction's location, by contrast, is a local lookup.
        assert_eq!(strategy(Capability::TransactionLocation), Strategy::Merge);
        assert_eq!(strategy(Capability::Broadcast), Strategy::Passthrough);
        assert_eq!(
            strategy(Capability::ReportedUpgrades),
            Strategy::Passthrough
        );
    }

    #[test]
    fn tier_splits_at_the_watermark() {
        assert_eq!(tier_of(h(5), h(10)), Tier::Finalised);
        assert_eq!(tier_of(h(10), h(10)), Tier::Finalised);
        assert_eq!(tier_of(h(11), h(10)), Tier::Recent);
    }
}
