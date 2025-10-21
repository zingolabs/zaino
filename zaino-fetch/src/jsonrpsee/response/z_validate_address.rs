//! Types associated with the `z_validateaddress` RPC request.

use std::convert::Infallible;

use serde::{de, Deserialize, Deserializer, Serialize};

use crate::jsonrpsee::connector::ResponseToError;

/// Response type for the `z_validateaddress` RPC.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum ZValidateAddress {
    /// Known response.
    Known(KnownZValidateAddress),

    /// Unknown response.
    Unknown,
}

impl ResponseToError for ZValidateAddress {
    type RpcError = Infallible;
}

/// Response type for the `z_validateaddress` RPC for zcashd.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KnownZValidateAddress {
    /// Valid response.
    Valid(ValidZValidateAddress),

    /// Invalid response.
    Invalid(InvalidZValidateAddress),
}

/// The "invalid" shape is just `{ "isvalid": false }`
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InvalidZValidateAddress {
    #[serde(rename = "isvalid")]
    is_valid: bool,
}

impl InvalidZValidateAddress {
    /// Create a new invalid response.
    pub fn new() -> Self {
        Self { is_valid: false }
    }
}

impl<'de> Deserialize<'de> for InvalidZValidateAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "isvalid")]
            is_valid: bool,
        }
        let Raw { is_valid } = Raw::deserialize(d)?;
        if is_valid {
            return Err(de::Error::custom("invalid branch must have isvalid=false"));
        }
        Ok(InvalidZValidateAddress { is_valid })
    }
}

/// Valid wrapper.
/// `#[serde(transparent)]` lets it serialize like the inner enum.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ValidZValidateAddress(AddressData);

impl<'de> Deserialize<'de> for ValidZValidateAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let inner = AddressData::deserialize(d)?;
        if !inner.common().is_valid {
            return Err(de::Error::custom("valid branch must have isvalid=true"));
        }
        Ok(ValidZValidateAddress(inner))
    }
}

/// The "valid" response. Can be P2PKH, P2SH, Sprout, Sapling, or Unified.
impl ValidZValidateAddress {
    /// Creates a response for a P2PKH address.
    pub fn p2pkh(address: impl Into<String>) -> Self {
        Self(AddressData::P2pkh {
            common: CommonValidFields::valid(address),
            is_mine: IsMine::Unknown,
        })
    }

    /// Creates a response for a P2SH address.
    pub fn p2sh(address: impl Into<String>) -> Self {
        Self(AddressData::P2sh {
            common: CommonValidFields::valid(address),
            is_mine: IsMine::Unknown,
        })
    }

    /// Creates a response for a Sprout address.
    pub fn sprout(
        address: impl Into<String>,
        paying_key: impl Into<String>,
        transmission_key: impl Into<String>,
    ) -> Self {
        Self(AddressData::Sprout {
            common: CommonValidFields::valid(address),
            paying_key: paying_key.into(),
            transmission_key: transmission_key.into(),
            is_mine: IsMine::Unknown,
        })
    }

    /// Creates a response for a Sapling address.
    pub fn sapling(
        address: impl Into<String>,
        diversifier: impl Into<String>,
        diversified_transmission_key: impl Into<String>,
    ) -> Self {
        Self(AddressData::Sapling {
            common: CommonValidFields::valid(address),
            diversifier: diversifier.into(),
            diversified_transmission_key: diversified_transmission_key.into(),
            is_mine: IsMine::Unknown,
        })
    }

    /// Creates a response for a Unified address.
    pub fn unified(address: impl Into<String>) -> Self {
        Self(AddressData::Unified {
            common: CommonValidFields::valid(address),
        })
    }

    /// Optional setters (mirror zcashd’s conditional fields)
    pub fn with_legacy_type(mut self, t: ZValidateAddressType) -> Self {
        self.common_mut().legacy_type = Some(t);
        self
    }

    /// Adds an `ismine` field.
    pub fn with_is_mine(mut self, v: IsMine) -> Self {
        match &mut self.0 {
            AddressData::P2pkh { is_mine, .. }
            | AddressData::P2sh { is_mine, .. }
            | AddressData::Sprout { is_mine, .. }
            | AddressData::Sapling { is_mine, .. } => *is_mine = v,
            AddressData::Unified { .. } => { /* UA has no `ismine` in zcashd */ }
        }
        self
    }

    /// Returns the address.
    pub fn address(&self) -> &str {
        self.common().address.as_str()
    }

    /// Returns the address type.
    pub fn address_type(&self) -> ZValidateAddressType {
        match &self.0 {
            AddressData::P2pkh { .. } => ZValidateAddressType::P2pkh,
            AddressData::P2sh { .. } => ZValidateAddressType::P2sh,
            AddressData::Sprout { .. } => ZValidateAddressType::Sprout,
            AddressData::Sapling { .. } => ZValidateAddressType::Sapling,
            AddressData::Unified { .. } => ZValidateAddressType::Unified,
        }
    }

    /// Returns the legacy field for the address type.
    pub fn legacy_type(&self) -> Option<ZValidateAddressType> {
        self.common().legacy_type
    }

    /// Returns the `ismine` field.
    pub fn is_mine(&self) -> IsMine {
        match &self.0 {
            AddressData::P2pkh { is_mine, .. }
            | AddressData::P2sh { is_mine, .. }
            | AddressData::Sprout { is_mine, .. }
            | AddressData::Sapling { is_mine, .. } => is_mine.clone(),
            AddressData::Unified { .. } => IsMine::Unknown,
        }
    }

    /// Returns the `payingkey` and `transmissionkey` fields.
    pub fn sprout_keys(&self) -> Option<(&str, &str)> {
        if let AddressData::Sprout {
            paying_key,
            transmission_key,
            ..
        } = &self.0
        {
            Some((paying_key.as_str(), transmission_key.as_str()))
        } else {
            None
        }
    }

    /// Returns the `diversifier` and `diversifiedtransmissionkey` fields.
    pub fn sapling_keys(&self) -> Option<(&str, &str)> {
        if let AddressData::Sapling {
            diversifier,
            diversified_transmission_key,
            ..
        } = &self.0
        {
            Some((diversifier.as_str(), diversified_transmission_key.as_str()))
        } else {
            None
        }
    }

    fn common(&self) -> &CommonValidFields {
        match &self.0 {
            AddressData::P2pkh { common, .. }
            | AddressData::P2sh { common, .. }
            | AddressData::Sprout { common, .. }
            | AddressData::Sapling { common, .. }
            | AddressData::Unified { common, .. } => common,
        }
    }
    fn common_mut(&mut self) -> &mut CommonValidFields {
        match &mut self.0 {
            AddressData::P2pkh { common, .. }
            | AddressData::P2sh { common, .. }
            | AddressData::Sprout { common, .. }
            | AddressData::Sapling { common, .. }
            | AddressData::Unified { common, .. } => common,
        }
    }
}

/// Common fields that appear for all valid responses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommonValidFields {
    /// Always `true`.
    #[serde(rename = "isvalid")]
    pub is_valid: bool,

    /// The address original provided.
    pub address: String,

    /// Deprecated alias for the type. Only present if the node exposes it.
    #[serde(rename = "type")]
    pub legacy_type: Option<ZValidateAddressType>,
}

impl CommonValidFields {
    fn valid(address: impl Into<String>) -> Self {
        Self {
            is_valid: true,
            address: address.into(),
            legacy_type: None,
        }
    }
}

/// `ismine` wrapper. Originally used by `zcashd`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "Option<bool>", into = "Option<bool>")]
pub enum IsMine {
    /// The address is in the wallet.
    Mine,

    /// The address is not in the wallet.
    NotMine,

    /// Unknown.
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
/// Note that the `ismine` field is only present for `zcashd`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "address_type", rename_all = "lowercase")]
enum AddressData {
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

impl AddressData {
    fn common(&self) -> &CommonValidFields {
        match self {
            AddressData::P2pkh { common, .. }
            | AddressData::P2sh { common, .. }
            | AddressData::Sprout { common, .. }
            | AddressData::Sapling { common, .. }
            | AddressData::Unified { common, .. } => common,
        }
    }
}

/// Address types returned by `zcashd`.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ZValidateAddressType {
    /// Transparent P2PKH
    P2pkh,

    /// Transparent P2SH
    P2sh,

    /// Sprout
    Sprout,

    /// Sapling
    Sapling,

    /// Unified
    Unified,
}
