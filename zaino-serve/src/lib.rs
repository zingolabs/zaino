//! Holds a gRPC server capable of servicing clients over both https and the nym mixnet.
//!
//! Also holds the rust implementations of the LightWallet Service (CompactTxStreamerServer) and (eventually) Darkside RPCs (DarksideTxStremerServer).

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod rpc;
pub mod server;
pub(crate) mod utils;

