//! Lightwallet RPC implementations.

#[cfg(not(feature = "nym_poc"))]
pub mod service;

#[cfg(feature = "nym_poc")]
pub mod nymwalletservice;

pub mod nymservice;
