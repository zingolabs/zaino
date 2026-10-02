//! The node-RPC / explorer serving use case.

use crate::controls::{Broadcast, MempoolSubscribe, TakeSnapshot, TipSubscribe};
use crate::node_status::NodeStatusRead;
use crate::read_sets::NodeRpcReads;

use super::{Serves, UseCase};

/// Node-RPC / explorer serving: raw blocks, pool-decomposed transactions and
/// spend lookups the wallet-shaped consumers never need, the chain-info
/// aggregate, and the node-operator status read from the validator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeRpc;

impl UseCase for NodeRpc {
    const NAME: &'static str = "node-rpc";
}

/// The node-RPC service: the read-set over a pin, plus broadcast, the
/// subscriptions, and the typed node-status read (info/mining/peers/solps).
pub trait NodeRpcService:
    TakeSnapshot<Snapshot: NodeRpcReads> + Broadcast + MempoolSubscribe + TipSubscribe + NodeStatusRead
{
}
impl<T> NodeRpcService for T where
    T: TakeSnapshot<Snapshot: NodeRpcReads>
        + Broadcast
        + MempoolSubscribe
        + TipSubscribe
        + NodeStatusRead
{
}

impl<S: NodeRpcService> Serves<NodeRpc> for S {}
