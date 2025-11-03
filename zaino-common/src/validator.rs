//! Validator type for Zaino configuration.

// use serde::{Deserialize, Serialize};
// use zebra_chain::parameters::testnet::ConfiguredActivationHeights;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Validator (full-node) type for Zaino configuration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ValidatorConfig {
    /// Full node / validator gprc listen port.
    pub validator_grpc_listen_address: SocketAddr,
    /// Full node / validator listen port.
    pub validator_jsonrpc_listen_address: SocketAddr,
    /// Path to the validator cookie file. Enable validator rpc cookie authentication with Some.
    pub validator_cookie_path: Option<PathBuf>,
    /// Full node / validator Username.
    pub validator_user: Option<String>,
    /// full node / validator Password.
    pub validator_password: Option<String>,
}
