//! The node-RPC / explorer use case.
//!
//! One deployment so far, [`NodeRpcLocal`], which serves transparent address
//! history and spend lookups from Zaino's own indexes and relays every other node
//! and chain read to the validator. Its validator floor is [`NodeRpcSource`].

mod local;

pub use local::{NodeRpcLocal, NodeRpcSource};
