//! Types associated with the `z_validateaddress` RPC request.

use serde::{Deserialize, Serialize};
use zebra_rpc::client::ZValidateAddressResponse;

/// Response type for the `z_validateaddress` RPC.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum ZValidateAddress {
    Zcashd(ZcashdZValidateAddress),
    Zebrad(ZValidateAddressResponse),
    Unknown,
}

/// Response type for the `z_validateaddress` RPC for zcashd.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ZcashdZValidateAddress {
    Valid(ValidZcashdZValidateAddress),
    Invalid(InvalidZcashdZValidateAddress),
}

/// The "invalid" shape is just `{ "isvalid": false }`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvalidZcashdZValidateAddress {
    #[serde(rename = "isvalid")]
    pub is_valid: bool,
}

/// Common fields that appear for all valid responses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommonValidFields {
    /// Always `true`.
    #[serde(rename = "isvalid")]
    pub is_valid: bool,

    pub address: String,

    /// Deprecated alias for the type. Only present if the node exposes it.
    #[serde(rename = "type")]
    pub legacy_type: Option<ZValidateAddressType>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "Option<bool>", into = "Option<bool>")]
pub enum IsMine {
    Mine,
    NotMine,
    Unknown,
}

impl Default for IsMine {
    fn default() -> Self {
        IsMine::Unknown
    }
}

impl From<Option<bool>> for IsMine {
    fn from(b: Option<bool>) -> Self {
        match b {
            Some(true) => IsMine::Mine,
            Some(false) => IsMine::NotMine,
            None => IsMine::Unknown,
        }
    }
}

impl From<IsMine> for Option<bool> {
    fn from(v: IsMine) -> Self {
        match v {
            IsMine::Mine => Some(true),
            IsMine::NotMine => Some(false),
            IsMine::Unknown => None,
        }
    }
}

/// Response for the Valid branch of the `z_validateaddress` RPC.
/// Note that the `ismine` field is only present if the node exposes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "address_type", rename_all = "lowercase")]
pub enum ValidZcashdZValidateAddress {
    /// Transparent P2PKH
    P2pkh {
        #[serde(flatten)]
        common: CommonValidFields,
        #[serde(rename = "ismine", default)]
        is_mine: IsMine,
    },

    /// Transparent P2SH
    P2sh {
        #[serde(flatten)]
        common: CommonValidFields,
        #[serde(rename = "ismine", default)]
        is_mine: IsMine,
    },

    /// Sprout address type
    Sprout {
        #[serde(flatten)]
        common: CommonValidFields,
        #[serde(rename = "payingkey")]
        paying_key: String,
        #[serde(rename = "transmissionkey")]
        transmission_key: String,
        #[serde(rename = "ismine", default)]
        is_mine: IsMine,
    },

    /// Sapling address type
    Sapling {
        #[serde(flatten)]
        common: CommonValidFields,
        /// Hex of the diversifier `d`
        diversifier: String,
        /// Hex of `pk_d`
        #[serde(rename = "diversifiedtransmissionkey")]
        diversified_transmission_key: String,
        #[serde(rename = "ismine", default)]
        is_mine: IsMine,
    },

    /// Unified Address (UA). `zcashd` currently returns no extra fields for UA.
    Unified {
        #[serde(flatten)]
        common: CommonValidFields,
    },
}

/// Address types returned by `zcashd`.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ZValidateAddressType {
    P2pkh,
    P2sh,
    Sprout,
    Sapling,
    Unified,
}
