//! Addon reverse indexes over the finalised block stream.
//!
//! Each is a *synthetic* index the sync engine extracts during freeze/bulk-build.
//! They are **addons on the spine** ([`crate::spine`]): a deployment builds only
//! the ones it serves, selected by feature or config. Not building one means:
//!
//! - less work per block — the delta is never extracted (work is a property of
//!   the index set), and
//! - the capability is absent — at the **type level** (the trait isn't
//!   implemented, so a consumer can't even ask) or, for a runtime toggle, via a
//!   `NotEnabled` error.
//!
//! Privacy: the address index is privacy-sensitive and belongs behind a
//! **non-default** feature flag.

mod address;
mod spend;
mod tx_location;

pub use address::AddressIndex;
pub use spend::SpendIndex;
pub use tx_location::TxLocationIndex;
