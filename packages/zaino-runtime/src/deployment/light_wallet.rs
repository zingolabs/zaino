//! The light-wallet use case and what its deployments share.
//!
//! One use case, served more than one way. What every light-wallet deployment
//! requires of the validator — the indexed assembly's floor plus the ports the
//! wallet's non-address reads always relay — is the shared base
//! [`LightWalletSource`]; a deployment that also relays address queries extends
//! it with the address source ports in its own file, and one that serves address
//! history locally requires exactly the base.

use zaino_source::{
    GetMempoolCompactTransaction, GetMempoolSourceTip, GetMempoolTxids, GetRawMempoolTransaction,
    GetSubtreeRoots, GetTransaction, GetTreestate, SendRawTransaction,
};

use crate::deployment::IndexedSource;

mod local;
mod passthrough;

pub use local::LightWalletLocal;
pub use passthrough::{LightWalletPassthrough, LightWalletPassthroughSource};

/// The floor every light-wallet deployment requires of the validator: the
/// indexed assembly's own floor ([`IndexedSource`]) plus the ports the wallet's
/// non-address reads relay through — broadcast, the mempool reads, raw
/// transactions, treestate and subtree roots.
///
/// A deployment that relays address queries too extends this with the address
/// source ports (see the passthrough deployment); one that serves address history
/// locally requires exactly this.
///
/// Hand-kept beside the deployments rather than derived, because Rust cannot
/// compute "the union of the source bounds of the impls a routing selects".
/// Safe to be wrong in one direction: a port missing here fails the demand bound
/// at the wiring, naming it.
pub trait LightWalletSource:
    IndexedSource
    + SendRawTransaction
    + GetMempoolTxids
    + GetMempoolSourceTip
    + GetRawMempoolTransaction
    + GetMempoolCompactTransaction
    + GetTransaction
    + GetTreestate
    + GetSubtreeRoots
{
}
impl<S> LightWalletSource for S where
    S: IndexedSource
        + SendRawTransaction
        + GetMempoolTxids
        + GetMempoolSourceTip
        + GetRawMempoolTransaction
        + GetMempoolCompactTransaction
        + GetTransaction
        + GetTreestate
        + GetSubtreeRoots
{
}
