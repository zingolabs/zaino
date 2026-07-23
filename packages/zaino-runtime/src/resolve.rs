//! Read-composition policy.
//!
//! How a request is answered from the components: **route** (one tier by
//! height), **merge** (both tiers), or **passthrough** (the validator). Kept
//! here, in one place, so neither the supervisor nor the per-capability
//! type-specific code re-derives the decision — one policy, tested once
//! (locality of correctness).

use zaino_core::{Capability, Height, Utxo};

use crate::config::RuntimeConfig;

/// Which local tier owns a routed read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Finalised,
    Recent,
}

/// A capability's inherent composition strategy.
// Passthrough policy (Strategy / strategy / passthrough_allowed): exercised by
// the tests below; wired into reads when full-block/raw-tx passthrough lands.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    /// One tier, chosen by height at the watermark (block, compact, treestate).
    Route,
    /// Both tiers combined (address history, spend status).
    Merge,
    /// Not stored — the validator answers, keyed by hash (full block, raw tx).
    Passthrough,
}

/// The static strategy for each capability.
#[allow(dead_code)]
pub(crate) fn strategy(cap: Capability) -> Strategy {
    match cap {
        Capability::Blocks | Capability::Treestate | Capability::SubtreeRoots => Strategy::Route,
        Capability::AddressHistory | Capability::SpendStatus => Strategy::Merge,
        Capability::Transactions
        | Capability::Mempool
        | Capability::Broadcast
        | Capability::ReportedUpgrades => Strategy::Passthrough,
    }
}

/// For a `Route` read: which tier owns `height`. The recent-not-ready case is
/// the snapshot's concern (it holds the pinned NFS `Option`); this is the pure
/// boundary at the watermark.
pub(crate) fn tier_of(height: Height, watermark: Height) -> Tier {
    if height <= watermark {
        Tier::Finalised
    } else {
        Tier::Recent
    }
}

/// Whether a `Passthrough` read may hit the validator now, given config (the
/// per-capability inherent decision is [`strategy`]; sync-state/coherence are
/// the snapshot's; this is the config gate).
pub(crate) fn passthrough_allowed(cap: Capability, cfg: &RuntimeConfig) -> bool {
    cfg.passthrough_enabled && cfg.capability_enabled(cap)
}

/// Combine an address's finalised and recent unspent outpoints (US-1.3).
///
/// TODO(US-1.3): also drop finalised UTXOs spent within the recent window —
/// needs recent spends-by-address from NFS.
pub(crate) fn merge_unspent(mut finalised: Vec<Utxo>, recent: Vec<Utxo>) -> Vec<Utxo> {
    finalised.extend(recent);
    finalised
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("valid height")
    }

    #[test]
    fn tier_splits_at_the_watermark() {
        assert_eq!(tier_of(h(99), h(100)), Tier::Finalised);
        assert_eq!(tier_of(h(100), h(100)), Tier::Finalised);
        assert_eq!(tier_of(h(101), h(100)), Tier::Recent);
    }

    #[test]
    fn strategy_per_capability() {
        assert_eq!(strategy(Capability::Blocks), Strategy::Route);
        assert_eq!(strategy(Capability::AddressHistory), Strategy::Merge);
        assert_eq!(strategy(Capability::Transactions), Strategy::Passthrough);
    }

    #[test]
    fn passthrough_is_gated_by_config() {
        let on = RuntimeConfig {
            passthrough_enabled: true,
        };
        let off = RuntimeConfig {
            passthrough_enabled: false,
        };
        assert!(passthrough_allowed(Capability::Transactions, &on));
        assert!(!passthrough_allowed(Capability::Transactions, &off));
    }
}
