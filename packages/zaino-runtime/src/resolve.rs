//! Read-composition policy.
//!
//! How a request is answered from the components: **route** (one tier by
//! height), **merge** (both tiers), or **passthrough** (the validator). Kept
//! here, in one place, so neither the supervisor nor the per-capability
//! type-specific code re-derives the decision — one policy, tested once
//! (locality of correctness).
//!
//! # Composition algebra
//!
//! Let `F` be the finalised tier, `N` the recent (non-finalised) tier, `V` the
//! validator; `wm` the finalised watermark, `tip` the recent tip. `F` answers
//! heights `(-inf, wm]`, `N` answers `(wm, tip]` — disjoint domains.
//!
//! ```text
//! route(h)       = F(h)                     if h <= wm
//!                = N(h)                      if h  > wm          -- disjoint by height
//! merge.unspent(a) = (F.unspent(a) \ spentN) ∪ N.created_unspent(a)
//!                                            -- spentN = outpoints spent in (wm, tip]
//! passthrough(id) = V(id)                    -- by immutable id, tier-independent
//! ```
//!
//! Each function below is one line of this algebra; the equation is the spec,
//! the code names are incidental to it.

use zaino_core::{Capability, Height, Outpoint, Utxo};

use crate::config::RuntimeConfig;

/// Which local tier owns a routed read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Finalised,
    Recent,
}

/// A capability's *local* serving strategy — how the index answers when it can.
///
/// Passthrough (the validator) is orthogonal: it is the only path for
/// capabilities with no local index (`Passthrough` below), *and* the fallback
/// for `Route`/`Merge` capabilities when their tier can't serve yet — gated by
/// [`passthrough_allowed`] and, inside a pinned snapshot, constrained to by-hash
/// reads for coherence.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    /// One tier, chosen by height at the watermark (block, compact, treestate).
    Route,
    /// Both tiers combined (address history, spend status).
    Merge,
    /// No local index — the validator is the only path, keyed by hash (full
    /// block, raw tx). The limit case of the fallback: "not-yet-built" = never.
    Passthrough,
}

/// The static local strategy for each capability.
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

/// The `route(h)` domain split: `F` owns `h <= wm`, `N` owns `h > wm`. The
/// recent-not-ready case is the snapshot's concern (it holds the pinned NFS
/// `Option`); this is the pure boundary at the watermark.
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

/// The outpoint identifying a UTXO. Business↔business (not a persistence/wire
/// boundary), so a plain fn rather than a `From`/`TryFrom`.
pub(crate) fn outpoint_of(utxo: &Utxo) -> Outpoint {
    Outpoint {
        txid: utxo.txid,
        index: utxo.output_index,
    }
}

/// `merge.unspent(a) = (F.unspent(a) \ spentN) ∪ N.created_unspent(a)` (US-1.3).
///
/// The finalised UTXOs *not* spent within the recent window (`\ spentN`), plus
/// the recent still-unspent creates (`∪ N.created_unspent`). `spent_in_window`
/// is `op ∈ spentN` — the NFS spend facet, supplied at the call site.
pub(crate) fn merge_unspent(
    finalised: Vec<Utxo>,
    recent: Vec<Utxo>,
    spent_in_window: impl Fn(&Outpoint) -> bool,
) -> Vec<Utxo> {
    let mut out: Vec<Utxo> = finalised
        .into_iter()
        .filter(|utxo| !spent_in_window(&outpoint_of(utxo)))
        .collect();
    out.extend(recent);
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use zaino_core::{Script, TransactionHash, TransparentAddress, Zatoshis};

    use super::*;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("valid height")
    }

    /// A minimal UTXO distinguished only by its outpoint `(txid, index)`.
    fn utxo(txid_tag: u8, index: u32) -> Utxo {
        Utxo {
            address: TransparentAddress::new("t1example".to_string()),
            txid: TransactionHash::from([txid_tag; 32]),
            output_index: index,
            script: Script::new(Vec::new()),
            satoshis: Zatoshis::new(1000).expect("valid amount"),
            height: h(50),
        }
    }

    #[test]
    fn merge_drops_finalised_outpoints_spent_in_the_recent_window() {
        // Finalised: A and B unspent as of the watermark.
        let a = utxo(0xA1, 0);
        let b = utxo(0xB2, 0);
        // Recent window: created C (still unspent) and spent A.
        let c = utxo(0xC3, 0);
        let spent: HashSet<Outpoint> = std::iter::once(outpoint_of(&a)).collect();

        let merged = merge_unspent(vec![a, b.clone()], vec![c.clone()], |op| spent.contains(op));

        // A dropped (spent in the window); B kept; C added.
        let got: HashSet<Outpoint> = merged.iter().map(outpoint_of).collect();
        let want: HashSet<Outpoint> = [outpoint_of(&b), outpoint_of(&c)].into_iter().collect();
        assert_eq!(got, want);
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
