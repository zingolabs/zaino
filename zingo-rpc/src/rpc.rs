//! Lightwattet RPC implementations.

#[cfg(not(feature = "nym_wallet"))]
pub mod service;

#[cfg(feature = "nym_wallet")]
pub mod nymwalletservice;
// pub mod nym_service_server;

// pub mod darkside;
