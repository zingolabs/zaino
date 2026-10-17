//! Node-operator status, read from the validator.
//!
//! Four facts about the node rather than the chain: its identity and
//! connections, its mining view, its peers, and the network solution rate.
//! Zaino indexes none of them, so all four are answered by passthrough.
//!
//! They are **typed**, not opaque: every `zaino-source` port beneath this one
//! already returns a domain type, so relaying a rendered string would discard
//! type information the layer below holds.
//!
//! One capability covers all four. They share the same availability — always
//! `Answerable::Live`, never height-bounded — so separate capability variants
//! would carry no information this one does not.

use std::future::Future;

use zaino_primitives::types::rpc::{MiningInfo, NetworkInfo, NodeInfo, PeerInfo};
use zaino_primitives::types::{Difficulty, Height};

/// Why a node-status read could not be answered.
///
/// Distinct from [`Transient`](crate::error::Transient), which is about
/// acquiring a snapshot: no snapshot is involved here. The distinction that
/// matters to a caller is whether the validator is *starting*, in which case
/// the same request succeeds shortly, or *unreachable*, which is a different
/// problem with a different fix.
///
/// The cause is held as a boxed `#[source]` rather than a `zaino-source` type
/// because this crate is the inner driving port and must not depend on the
/// driven one. Boxing keeps the chain intact without the dependency.
#[derive(Debug, thiserror::Error)]
pub enum NodeStatusError {
    /// The validator is running but not yet ready to describe itself.
    #[error("validator not ready")]
    NotReady,
    /// The validator could not be reached, or its answer was unusable.
    #[error("validator unreachable")]
    Unreachable {
        /// The underlying source-layer failure.
        #[source]
        cause: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl NodeStatusError {
    /// An unreachable validator, preserving `cause` in the source chain.
    pub fn unreachable<E>(cause: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Unreachable {
            cause: Box::new(cause),
        }
    }
}

/// Node-operator status, relayed from the validator.
///
/// A control rather than a snapshot read: these answers come from the source
/// live, and nothing pins them to a chain view.
pub trait NodeStatusRead: Send + Sync {
    /// `getinfo`: version, connections, fee floors and health.
    fn node_info(&self) -> impl Future<Output = Result<NodeInfo, NodeStatusError>> + Send;

    /// `getmininginfo`: the validator's mining view.
    fn mining_info(&self) -> impl Future<Output = Result<MiningInfo, NodeStatusError>> + Send;

    /// `getpeerinfo`: the validator's connected peers.
    fn peer_info(&self) -> impl Future<Output = Result<Vec<PeerInfo>, NodeStatusError>> + Send;

    /// `getnetworksolps`: the network solution rate in solutions per second,
    /// averaged over `blocks` ending at `height`. `None` for either asks the
    /// validator for its own default — its window, and its tip.
    fn network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<Height>,
    ) -> impl Future<Output = Result<u64, NodeStatusError>> + Send;

    /// `getdifficulty`: the current difficulty, as a multiple of the network
    /// minimum.
    fn difficulty(&self) -> impl Future<Output = Result<Difficulty, NodeStatusError>> + Send;

    /// `getnetworkinfo`: the validator's peer-to-peer network view.
    fn network_info(&self) -> impl Future<Output = Result<NetworkInfo, NodeStatusError>> + Send;

    /// `ping`: confirm the validator is responsive. `Ok(())` on a successful
    /// response, which the node-RPC surface renders as JSON `null`.
    fn ping(&self) -> impl Future<Output = Result<(), NodeStatusError>> + Send;
}
