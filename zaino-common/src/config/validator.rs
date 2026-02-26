//! Validator type for Zaino configuration.

// use serde::{Deserialize, Serialize};
// use zebra_chain::parameters::testnet::ConfiguredActivationHeights;
use std::path::PathBuf;

/// Validator (full-node) connection settings.
///
/// Configures how Zaino connects to the backing validator (Zebra or Zcashd).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ValidatorConfig {
    /// Validator gRPC listen address (Zebra only).
    ///
    /// Must be a "private" address as defined in IETF RFC 1918 (IPv4) or RFC 4193 (IPv6).
    /// Cookie or user/password authentication is recommended for non-localhost addresses.
    pub validator_grpc_listen_address: Option<String>,
    /// Validator JSON-RPC listen address.
    ///
    /// Supports hostname:port or ip:port format.
    /// Must be a "private" address as defined in IETF RFC 1918 (IPv4) or RFC 4193 (IPv6).
    pub validator_jsonrpc_listen_address: String,
    /// Path to the validator cookie file for cookie-based authentication.
    ///
    /// When set, enables cookie authentication instead of user/password.
    pub validator_cookie_path: Option<PathBuf>,
    /// Validator RPC username for user/password authentication.
    pub validator_user: Option<String>,
    /// Validator RPC password for user/password authentication.
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
