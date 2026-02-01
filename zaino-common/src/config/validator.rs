//! Validator type for Zaino configuration.

// use serde::{Deserialize, Serialize};
// use zebra_chain::parameters::testnet::ConfiguredActivationHeights;
use std::path::PathBuf;

/// Validator (full-node) type for Zaino configuration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ValidatorConfig {
    /// Full node / validator gprc listen port. Only exists for zebra
    pub validator_grpc_listen_address: Option<String>,
    /// Full node / validator listen address (supports hostname:port or ip:port format).
    pub validator_jsonrpc_listen_address: String,
    /// Path to the validator cookie file. Enable validator rpc cookie authentication with Some.
    pub validator_cookie_path: Option<PathBuf>,
    /// Full node / validator Username.
    pub validator_user: Option<String>,
    /// full node / validator Password.
    pub validator_password: Option<String>,
}

/// Required by `#[serde(default)]` to fill missing fields when deserializing partial TOML configs.
impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            validator_grpc_listen_address: Some("127.0.0.1:18230".to_string()),
            validator_jsonrpc_listen_address: "127.0.0.1:18232".to_string(),
            validator_cookie_path: None,
            validator_user: Some("xxxxxx".to_string()),
            validator_password: Some("xxxxxx".to_string()),
        }
    }
}
