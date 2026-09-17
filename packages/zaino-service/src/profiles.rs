//! Consumer-facing capability bundles.
//!
//! Two altitudes, both derived from the public use cases (full-wallet library,
//! light-wallet serving, node-RPC/explorer serving):
//!
//! - **Read-sets** ([`read_sets`]) name *the demand* — which reads a use case
//!   pulls through a pinned view. They are the `required` capability set of the
//!   availability model, made first-class. Coherence is orthogonal and asserted
//!   once, by [`TakeSnapshot`]'s associated-type bound (`type Snapshot:
//!   Snapshot`).
//! - **Service profiles** ([`service`]) compose a use case's read-set (via the
//!   pin) with the control capabilities it needs.
//!
//! Both altitudes are blanket-implemented: a type *is* a bundle exactly when it
//! has the constituent capabilities — the demand-first, structural rule the
//! source adapters also follow. So the runtime's single concrete engine
//! satisfies every profile, and each adapter depends only on the one it needs.
//!
//! [`TakeSnapshot`]: crate::TakeSnapshot

mod read_sets;
mod service;

pub use read_sets::{FullWalletReads, LightWalletReads, NodeRpcReads, WalletReadCore};
pub use service::{LightServeService, NodeRpcService, WalletLibService};
