//! Response bodies, in the field names and encodings zcashd uses.
//!
//! Zatoshi quantities render as integers: `balance` is supply-bounded and fits
//! `u64`, while `received` is a lifetime flow total that is not supply-bounded
//! and so renders from `u128`.

use serde::Serialize;

/// The `getaddressbalance` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressBalanceResponse {
    /// Total currently held, in zatoshis.
    pub balance: u64,
    /// Lifetime gross receipts, in zatoshis.
    pub received: u128,
}

/// One entry of the `getaddressdeltas` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressDeltaEntry {
    /// Signed change in zatoshis — negative for a spend.
    pub satoshis: i64,
    /// The transaction that caused the delta, as hex.
    pub txid: String,
    /// Input or output index within the transaction.
    pub index: u32,
    /// Position of the transaction within its block, when the source knows it.
    #[serde(rename = "blockindex", skip_serializing_if = "Option::is_none")]
    pub block_index: Option<u32>,
    /// Block height of the delta.
    pub height: u32,
    /// The transparent address affected.
    pub address: String,
}

/// The range a `chainInfo` request echoes alongside its deltas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeltaRange {
    /// First height included.
    pub start: u32,
    /// Last height included.
    pub end: u32,
}

/// The `getaddressdeltas` response. `range` appears only when the request asked
/// for `chainInfo` and a query actually ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddressDeltasResponse {
    /// The deltas, in `(height, blockindex, index)` order.
    pub deltas: Vec<AddressDeltaEntry>,
    /// The queried range, when `chainInfo` was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<DeltaRange>,
}

/// The `validateaddress` response. zcashd reports an unusable address as
/// `isvalid: false` with no other fields, rather than as an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidateAddressResponse {
    /// Whether the address is a transparent address on the queried network.
    pub isvalid: bool,
    /// The address as supplied, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Whether the address is pay-to-script-hash, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isscript: Option<bool>,
}

/// The `z_validateaddress` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZValidateAddressResponse {
    /// Whether the address is one Zaino classifies on the queried network.
    pub isvalid: bool,
    /// The address, re-encoded for the queried network, when valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// zcashd's address-kind tag: `p2pkh`, `p2sh`, `sapling` or `unified`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_type: Option<String>,
    /// Sapling diversifier as hex, for a Sapling address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diversifier: Option<String>,
    /// Sapling `pk_d` as hex, for a Sapling address.
    #[serde(
        rename = "diversifiedtransmissionkey",
        skip_serializing_if = "Option::is_none"
    )]
    pub diversified_transmission_key: Option<String>,
}

/// The `z_listunifiedreceivers` response: one field per receiver kind the
/// unified address bundles, absent when it carries none of that kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnifiedReceiversResponse {
    /// Orchard receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchard: Option<String>,
    /// Sapling receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sapling: Option<String>,
    /// Transparent pay-to-public-key-hash receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2pkh: Option<String>,
    /// Transparent pay-to-script-hash receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2sh: Option<String>,
}
