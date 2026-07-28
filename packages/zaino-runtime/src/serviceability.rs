//! Serviceability derivation.
//!
//! Projects the runtime's live state onto a [`ServiceabilityManifest`]: for each
//! capability this deployment serves, the height it is answerable up to *now*
//! (`None` = known but not answerable now — the recent window is still syncing,
//! or the capability is config-gated off). Pure, so it's tested in isolation;
//! the `Serviceable` impl (in `runtime.rs`) just gathers state and calls it.
//!
//! Drift-free by construction: the capability list is `Capability::iter()`
//! (generated from the variants), and per-capability handling routes through
//! `resolve::strategy` — an exhaustive match. A new capability is forced through
//! `strategy` by the compiler and appears in the manifest automatically.

use strum::IntoEnumIterator;

use zaino_core::{Capability, Height, ServiceabilityManifest};

use crate::config::RuntimeConfig;
use crate::resolve::{self, Strategy};

/// The live state a manifest is derived from.
pub(crate) struct State {
    /// Top of the finalised (append-only) state.
    pub finalized_tip: Height,
    /// `Some(tip)` when the NFS window is ready; `None` while it is syncing.
    pub nfs_tip: Option<Height>,
}

/// Derive the manifest: one entry per served capability, each with the height it
/// is answerable up to now (`None` = not answerable now).
pub(crate) fn manifest(cfg: &RuntimeConfig, state: &State) -> ServiceabilityManifest {
    let answerable = Capability::iter()
        .filter(|&cap| cfg.capability_enabled(cap))
        .map(|cap| (cap, answerable_to(cap, cfg, state)))
        .collect();
    ServiceabilityManifest { answerable }
}

/// The height `cap` is answerable up to now, per its composition strategy.
fn answerable_to(cap: Capability, cfg: &RuntimeConfig, state: &State) -> Option<Height> {
    match resolve::strategy(cap) {
        // Finalised is answerable up to the watermark; the recent window extends
        // it to the tip once ready (partial service while syncing — US-0.2).
        Strategy::Route => Some(state.nfs_tip.unwrap_or(state.finalized_tip)),
        // Needs both tiers coherent → answerable only once the window is ready.
        Strategy::Merge => state.nfs_tip,
        // The validator answers by hash, independent of sync — gated by config.
        // (Broadcast/mempool/reported-upgrades are coarsely grouped as
        // `Passthrough` for now; refine when the control caps are modelled.)
        Strategy::Passthrough => resolve::passthrough_allowed(cap, cfg)
            .then(|| state.nfs_tip.unwrap_or(state.finalized_tip)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("valid height")
    }

    fn answerable(m: &ServiceabilityManifest, cap: Capability) -> Option<Height> {
        m.answerable
            .iter()
            .find(|(c, _)| *c == cap)
            .unwrap_or_else(|| panic!("{cap:?} absent from manifest"))
            .1
    }

    #[test]
    fn ready_serves_every_tier_to_the_tip() {
        let cfg = RuntimeConfig {
            passthrough_enabled: true,
        };
        let state = State {
            finalized_tip: h(100),
            nfs_tip: Some(h(150)),
        };
        let m = manifest(&cfg, &state);
        assert_eq!(answerable(&m, Capability::Blocks), Some(h(150))); // route
        assert_eq!(answerable(&m, Capability::AddressHistory), Some(h(150))); // merge
        assert_eq!(answerable(&m, Capability::Transactions), Some(h(150))); // passthrough
    }

    #[test]
    fn syncing_serves_finalised_route_but_not_the_merge() {
        let cfg = RuntimeConfig {
            passthrough_enabled: true,
        };
        let state = State {
            finalized_tip: h(100),
            nfs_tip: None,
        };
        let m = manifest(&cfg, &state);
        // Route degrades to the watermark; merge is unanswerable; passthrough is
        // sync-independent (best-known height is the watermark).
        assert_eq!(answerable(&m, Capability::Blocks), Some(h(100)));
        assert_eq!(answerable(&m, Capability::AddressHistory), None);
        assert_eq!(answerable(&m, Capability::Transactions), Some(h(100)));
    }

    #[test]
    fn passthrough_off_makes_passthrough_caps_unanswerable() {
        let cfg = RuntimeConfig {
            passthrough_enabled: false,
        };
        let state = State {
            finalized_tip: h(100),
            nfs_tip: Some(h(150)),
        };
        let m = manifest(&cfg, &state);
        assert_eq!(answerable(&m, Capability::Transactions), None);
        // Local tiers are unaffected by the passthrough toggle.
        assert_eq!(answerable(&m, Capability::Blocks), Some(h(150)));
        assert_eq!(answerable(&m, Capability::AddressHistory), Some(h(150)));
    }
}
