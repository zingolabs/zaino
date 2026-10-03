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

/// The `getaddresstxids` object parameter, as `zcashex` sends it: one positional
/// object carrying the addresses and an optional inclusive height window.
#[derive(Debug, Clone, Deserialize)]
pub struct AddressTxidsParam {
    /// The transparent addresses to query.
    pub addresses: Vec<String>,
    /// First height to include, inclusive. Absent defaults to the serviceable
    /// floor.
    #[serde(default)]
    pub start: Option<u32>,
    /// Last height to include, inclusive. Absent defaults to the serviceable tip.
    #[serde(default)]
    pub end: Option<u32>,
}

/// The optional third parameter of `getblockhashes`, the
/// `{"noOrphans": ..., "logicalTimes": ...}` object `zcashex` sends. The whole
/// object is optional (`[high, low]` is a valid call), and each key within it is
/// optional too, defaulting to zcashd's defaults.
///
/// `noOrphans` is deliberately not modelled: zcashd uses it to restrict the
/// timestamp index to the active chain, but Zaino's local read serves the active
/// chain by construction, so the option is already satisfied and the key is
/// accepted and ignored (unknown keys are, by default, dropped). Only
/// `logicalTimes` changes the response.
#[derive(Debug, Clone, Deserialize)]
pub struct GetBlockHashesOptions {
    /// zcashd's `logicalTimes`: when true, return `{blockhash, logicalts}`
    /// objects instead of bare hash strings. Defaults to false.
    #[serde(default, rename = "logicalTimes")]
    pub logical_times: bool,
}
