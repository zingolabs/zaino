//! Request parameters, as the explorer's client sends them.
//!
//! Shapes follow `nighthawk-apps/zcashex`, the client real callers use, rather
//! than the prose in the zcashd RPC docs: the address RPCs take a single object
//! parameter, not a positional list.

use serde::Deserialize;

/// The `{"addresses": [...]}` object the address RPCs take as their one
/// positional parameter.
#[derive(Debug, Clone, Deserialize)]
pub struct AddressesParam {
    /// The transparent addresses to query.
    pub addresses: Vec<String>,
}

/// The `getaddressdeltas` object parameter, as `zcashex` sends it: one
/// positional object, not a positional list.
#[derive(Debug, Clone, Deserialize)]
pub struct AddressDeltasParam {
    /// The transparent addresses to query.
    pub addresses: Vec<String>,
    /// First height to include, inclusive.
    #[serde(default)]
    pub start: Option<u32>,
    /// Last height to include, inclusive.
    #[serde(default)]
    pub end: Option<u32>,
    /// Whether the response wraps the list with the queried range.
    #[serde(default, rename = "chainInfo")]
    pub chain_info: bool,
}
