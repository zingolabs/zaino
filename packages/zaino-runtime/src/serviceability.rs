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

use crate::config::{CapabilitySet, RuntimeConfig};
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
pub(crate) fn manifest(
    cfg: &RuntimeConfig,
    served: &CapabilitySet,
    state: &State,
) -> ServiceabilityManifest {
    let answerable = Capability::iter()
        .filter(|&cap| is_served(cap, cfg, served))
        .map(|cap| (cap, answerable_to(cap, state)))
        .collect();
    ServiceabilityManifest { answerable }
}

/// Whether the deployment offers `cap` at all — the *same* decision the reads
/// make, so the manifest can't advertise what a read would refuse.
fn is_served(cap: Capability, cfg: &RuntimeConfig, served: &CapabilitySet) -> bool {
    match resolve::strategy(cap) {
        // Route reads ride the always-present spine.
        Strategy::Route => true,
        // Merge reads need an opted-in (type-gated, index-backed) capability.
        Strategy::Merge => served.contains(cap),
        // Passthrough reads need the validator enabled. (Broadcast/mempool/
        // reported-upgrades are coarsely grouped as `Passthrough` for now;
        // refine when the control caps are modelled.)
        Strategy::Passthrough => resolve::passthrough_allowed(cap, cfg),
    }
}

/// The height `cap` is answerable up to now, assuming it is served ([`is_served`]).
/// Per strategy (`wm` = finalised watermark, `tip` = recent tip, ⊥ = not now):
///
/// ```text
/// route       -> tip  if window ready else wm      -- finalised always, extended to tip
/// merge       -> tip  if window ready else ⊥       -- needs both tiers coherent
/// passthrough -> tip-or-wm                          -- by hash, sync-independent
/// ```
fn answerable_to(cap: Capability, state: &State) -> Option<Height> {
    match resolve::strategy(cap) {
        // Finalised is answerable up to the watermark; the recent window extends
        // it to the tip once ready (partial service while syncing — US-0.2). The
        // validator (passthrough) answers by hash, independent of sync.
        Strategy::Route | Strategy::Passthrough => {
            Some(state.nfs_tip.unwrap_or(state.finalized_tip))
        }
        // Needs both tiers coherent → answerable only once the window is ready.
        Strategy::Merge => state.nfs_tip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("valid height")
    }

    /// A served set that opts into the address merge (as a full deployment would
    /// via the type-gated assembler).
    fn serving_address() -> CapabilitySet {
        let mut s = CapabilitySet::default();
        s.insert(Capability::AddressHistory);
        s
    }

    /// `Some(height)` if present-and-answerable, `Some(None)` if present-but-not-
    /// now, `None` if the capability is **absent** (not served).
    fn entry(m: &ServiceabilityManifest, cap: Capability) -> Option<Option<Height>> {
        m.answerable.iter().find(|(c, _)| *c == cap).map(|(_, h)| *h)
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
        let m = manifest(&cfg, &serving_address(), &state);
        assert_eq!(entry(&m, Capability::Blocks), Some(Some(h(150)))); // route
        assert_eq!(entry(&m, Capability::AddressHistory), Some(Some(h(150)))); // merge
        assert_eq!(entry(&m, Capability::Transactions), Some(Some(h(150)))); // passthrough
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
        let m = manifest(&cfg, &serving_address(), &state);
        // Route degrades to the watermark; merge is present but unanswerable;
        // passthrough is sync-independent (best-known height is the watermark).
        assert_eq!(entry(&m, Capability::Blocks), Some(Some(h(100))));
        assert_eq!(entry(&m, Capability::AddressHistory), Some(None));
        assert_eq!(entry(&m, Capability::Transactions), Some(Some(h(100))));
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
        let m = manifest(&cfg, &serving_address(), &state);
        // Passthrough caps drop out of the manifest entirely (not served now).
        assert_eq!(entry(&m, Capability::Transactions), None);
        // Local tiers are unaffected by the passthrough toggle.
        assert_eq!(entry(&m, Capability::Blocks), Some(Some(h(150))));
        assert_eq!(entry(&m, Capability::AddressHistory), Some(Some(h(150))));
    }

    #[test]
    fn a_merge_cap_not_opted_in_is_absent() {
        let cfg = RuntimeConfig {
            passthrough_enabled: true,
        };
        let state = State {
            finalized_tip: h(100),
            nfs_tip: Some(h(150)),
        };
        // Empty served set — the deployment didn't opt into any merge cap.
        let m = manifest(&cfg, &CapabilitySet::default(), &state);
        // Absent entirely — not "present but None". A minimal deployment simply
        // doesn't claim address history.
        assert_eq!(entry(&m, Capability::AddressHistory), None);
        // Route/passthrough are unaffected.
        assert_eq!(entry(&m, Capability::Blocks), Some(Some(h(150))));
    }
}
