//! Holds tonic generated code for the lightwallet service RPCs and compact formats.
//!
//! * We plan to eventually rely on LibRustZcash's versions but hold our our here for development purposes.
//! * Currently only holds the lightwallet proto files.

#![forbid(unsafe_code)]

pub mod proto;

#[cfg(feature = "grpc_proxy_server")]
pub use prost;
#[cfg(feature = "grpc_proxy_server")]
pub use tonic;
