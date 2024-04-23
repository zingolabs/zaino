//! Zingo-Proxy gRPC Server.
//! NOTE: This is currently a very simple implementation meant only for development and testing, and in its current form should not be used to run mainnet nodes.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod grpc;
pub mod nym_server;
pub mod server;
