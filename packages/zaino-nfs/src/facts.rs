//! Re-derived transparent facts over the reorg window.
//!
//! Each answers a transparent-history question for the *recent* range, and — the
//! key asymmetry with the FS addon indexes — is **re-derived on demand from the
//! window's retained blocks**, not a persistent index. The window is small
//! (~100 blocks) and churns on every reorg, so there is nothing worth
//! materializing or rebuilding; a scan of what's already held answers it.
//!
//! Split out (mirroring the FS `indexes` split) for modelling clarity, and so a
//! disabled capability is off on *both* tiers: the same feature that omits the
//! FS index omits its NFS facet.

mod address;
mod spend;

pub use address::NfsAddressFacts;
pub use spend::NfsSpendFacts;
