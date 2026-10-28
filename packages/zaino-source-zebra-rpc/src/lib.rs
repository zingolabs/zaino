//! Zebra JSON-RPC adapter — implements zaino-source query traits
//! against a Zebra validator over JSON-RPC.
//!
//! Uses [`zaino_rpc::RpcClient`] for transport and Zebra's response
//! format for deserialization.

mod adapter;
mod parse;

pub use adapter::ZebraRpcAdapter;
