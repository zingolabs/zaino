//! Types associated with the `z_validateaddress` RPC request.
//!
//! # Deprecation
//!
//! Delegating address validation to a non-client actor encourages information
//! leakage. This service is only offered for bugwards compatibility, and
//! **WILL BE REMOVED**.

use std::collections::BTreeMap;

/// Deprecation notice for the `z_validateaddress` endpoint. Emitted at
/// runtime via `tracing::warn!` and referenced in doc comments. The
/// `#[deprecated]` attributes point to the tracking issue instead.
pub const DEPRECATION_NOTICE: &str = "z_validateaddress is deprecated: delegating address validation to a non-client actor encourages information leakage. This service is only offered for bugwards compatibility with zcashd, and WILL BE REMOVED.";

use serde::{
    de,
    ser::{SerializeMap, SerializeStruct},
    Deserialize, Deserializer, Serialize, Serializer,
};
use serde_json::Value;

use crate::jsonrpsee::connector::{ResponseToError, RpcError};

/// Error type for the `z_validateaddress` RPC.
#[derive(Debug, thiserror::Error)]
pub enum ZValidateAddressError {
    /// Invalid address encoding
    #[error("Invalid encoding: {0}")]
    InvalidEncoding(String),
}

/// Error returned by [`ValidZValidateAddress::validate`] when cross-field
/// invariants are violated. A valid Sapling address always has both a
/// diversifier and a pk_d; a partial response indicates a malformed upstream.
#[derive(Debug, thiserror::Error)]
enum SaplingKeysError {
    /// Diversifier is present but pk_d is missing.
    #[error("partial sapling keys: diversifier present ({diversifier:?}) but pk_d missing")]
    DiversifierOnly { diversifier: String },

    /// pk_d is present but diversifier is missing.
    #[error("partial sapling keys: pk_d present ({pkd:?}) but diversifier missing")]
    PkdOnly { pkd: String },
}

/// Response type for the `z_validateaddress` RPC.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum ZValidateAddressResponse {
    /// A response containing a known JSON schema.
    Known(KnownZValidateAddress),

    /// A response containing an unknown JSON schema.
    Unknown(BTreeMap<String, Value>),
}

impl ZValidateAddressResponse {
    /// Constructs a response with a [`ZValidateAddressResponse::Unknown`] schema.
    pub fn unknown() -> Self {
        ZValidateAddressResponse::Unknown(BTreeMap::new())
    }

    /// Constructs an invalid address object.
    pub fn invalid() -> Self {
        ZValidateAddressResponse::Known(KnownZValidateAddress::Invalid(
            InvalidZValidateAddress::new(),
        ))
    }

    /// Constructs a response for a valid P2PKH address.
    pub fn p2pkh(address: impl Into<String>) -> Self {
        ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(ValidZValidateAddress::p2pkh(
            address,
        )))
    }

    /// Constructs a response for a valid P2SH address.
    pub fn p2sh(address: impl Into<String>) -> Self {
        ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(ValidZValidateAddress::p2sh(
            address,
        )))
    }

    /// Constructs a response for a valid Sapling address.
    pub fn sapling(
        address: impl Into<String>,
        diversifier: Option<String>,
        diversified_transmission_key: Option<String>,
    ) -> Self {
        ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::sapling(address, diversifier, diversified_transmission_key),
        ))
    }

    /// Constructs a response for a valid Sprout address.
    pub fn sprout(
        address: impl Into<String>,
        paying_key: Option<String>,
        transmission_key: Option<String>,
    ) -> Self {
        ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::sprout(address, paying_key, transmission_key),
        ))
    }

    /// Constructs a response for a valid Unified address.
    pub fn unified(address: impl Into<String>) -> Self {
        ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::unified(address),
        ))
    }
}

impl ResponseToError for ZValidateAddressResponse {
    type RpcError = ZValidateAddressError;
}

impl TryFrom<RpcError> for ZValidateAddressError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        Err(value)
    }
}

/// An enum that represents the known JSON schema for the `z_validateaddress` RPC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KnownZValidateAddress {
    /// Valid response.
    Valid(ValidZValidateAddress),

    /// Invalid response.
    Invalid(InvalidZValidateAddress),
}

/// The "invalid" shape is just `{ "isvalid": false }`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct InvalidZValidateAddress;

impl InvalidZValidateAddress {
    /// Creates a new InvalidZValidateAddress.
    pub fn new() -> Self {
        Self
    }
}

impl Serialize for InvalidZValidateAddress {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut serialized_struct = s.serialize_struct("InvalidZValidateAddress", 1)?;
        serialized_struct.serialize_field("isvalid", &false)?;
        serialized_struct.end()
    }
}

impl<'de> Deserialize<'de> for InvalidZValidateAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(rename = "isvalid")]
            is_valid: bool,
        }
        let Raw { is_valid } = Raw::deserialize(d)?;
        if is_valid {
            Err(de::Error::custom("invalid branch must have isvalid=false"))
        } else {
            Ok(InvalidZValidateAddress)
        }
    }
}

// TODO: `AddressData` should probably be private and exposed through an `inner` accessor.
/// Represents the "valid" response. The other fields are part of [`AddressData`].
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ValidZValidateAddress(AddressData);

impl<'de> Deserialize<'de> for ValidZValidateAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let inner = AddressData::deserialize(d)?;
        if !inner.common().is_valid {
            return Err(de::Error::custom("valid branch must have isvalid=true"));
        }

        let result = ValidZValidateAddress(inner);
        result.validate().map_err(de::Error::custom)?;
        Ok(result)
    }
}

/// The "valid" response. Can be P2PKH, P2SH, Sprout, Sapling, or Unified.
impl ValidZValidateAddress {
    /// Creates a response for a P2PKH address.
    pub fn p2pkh(address: impl Into<String>) -> Self {
        Self(AddressData::P2pkh {
            common: CommonFields::valid(address, ZValidateAddressType::P2pkh),
        })
    }

    /// Creates a response for a P2SH address.
    pub fn p2sh(address: impl Into<String>) -> Self {
        Self(AddressData::P2sh {
            common: CommonFields::valid(address, ZValidateAddressType::P2sh),
        })
    }

    /// Creates a response for a Sprout address.
    pub fn sprout<T: Into<String>>(
        address: impl Into<String>,
        paying_key: Option<T>,
        transmission_key: Option<T>,
    ) -> Self {
        Self(AddressData::Sprout {
            common: CommonFields::valid(address, ZValidateAddressType::Sprout),
            paying_key: paying_key.map(|x| x.into()),
            transmission_key: transmission_key.map(|x| x.into()),
        })
    }

    /// Creates a response for a Sapling address.
    pub fn sapling<T: Into<String>>(
        address: impl Into<String>,
        diversifier: Option<T>,
        diversified_transmission_key: Option<T>,
    ) -> Self {
        Self(AddressData::Sapling {
            common: CommonFields::valid(address, ZValidateAddressType::Sapling),
            diversifier: diversifier.map(|x| x.into()),
            diversified_transmission_key: diversified_transmission_key.map(|x| x.into()),
        })
    }

    /// Creates a response for a Unified address.
    pub fn unified(address: impl Into<String>) -> Self {
        Self(AddressData::Unified {
            common: CommonFields::valid(address, ZValidateAddressType::Unified),
        })
    }

    /// Optional setters (mirror zcashd’s conditional fields)
    pub fn with_legacy_type(mut self, t: ZValidateAddressType) -> Self {
        self.common_mut().legacy_type = Some(t);
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

    /// Returns the `payingkey` and `transmissionkey` fields.
    pub fn sprout_keys(&self) -> Option<(&str, &str)> {
        if let AddressData::Sprout {
            paying_key: Some(paying_key),
            transmission_key: Some(transmission_key),
            ..
        } = &self.0
        {
            Some((paying_key.as_str(), transmission_key.as_str()))
        } else {
            None
        }
    }

    /// Returns the `diversifier` and `diversifiedtransmissionkey` fields.
    ///
    /// Returns `Some` only if both fields are present, `None` if both are
    /// absent (e.g. zebrad passthrough). Partial keys (one present, one
    /// absent) are rejected at deserialization time.
    pub fn sapling_keys(&self) -> Option<(&str, &str)> {
        if let AddressData::Sapling {
            diversifier: Some(diversifier),
            diversified_transmission_key: Some(diversified_transmission_key),
            ..
        } = &self.0
        {
            Some((diversifier.as_str(), diversified_transmission_key.as_str()))
        } else {
            None
        }
    }

    /// Validates cross-field invariants that cannot be enforced by the type
    /// system alone. Call this after deserialization and before trusting the
    /// data.
    ///
    /// Currently checks:
    /// - Sapling: diversifier and pk_d must be both present or both absent.
    ///   A valid Sapling address always has both components; a partial
    ///   response indicates a malformed upstream.
    fn validate(&self) -> Result<(), SaplingKeysError> {
        if let AddressData::Sapling {
            diversifier,
            diversified_transmission_key,
            ..
        } = &self.0
        {
            match (diversifier, diversified_transmission_key) {
                (Some(d), None) => {
                    return Err(SaplingKeysError::DiversifierOnly {
                        diversifier: d.clone(),
                    });
                }
                (None, Some(pk_d)) => {
                    return Err(SaplingKeysError::PkdOnly { pkd: pk_d.clone() });
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn common(&self) -> &CommonFields {
        match &self.0 {
            AddressData::P2pkh { common, .. }
            | AddressData::P2sh { common, .. }
            | AddressData::Sprout { common, .. }
            | AddressData::Sapling { common, .. }
            | AddressData::Unified { common, .. } => common,
        }
    }
    fn common_mut(&mut self) -> &mut CommonFields {
        match &mut self.0 {
            AddressData::P2pkh { common, .. }
            | AddressData::P2sh { common, .. }
            | AddressData::Sprout { common, .. }
            | AddressData::Sapling { common, .. }
            | AddressData::Unified { common, .. } => common,
        }
    }

    /// Returns the address data.
    pub fn inner(&self) -> &AddressData {
        &self.0
    }
}

/// Common fields that appear for all valid responses.
#[derive(Clone, Debug, PartialEq)]
pub struct CommonFields {
    is_valid: bool,

    /// The address originally provided.
    pub address: String,

    /// Deprecated alias for the type. Only present if the node exposes it.
    pub legacy_type: Option<ZValidateAddressType>,
}

impl CommonFields {
    fn valid(address: impl Into<String>, legacy_type: ZValidateAddressType) -> Self {
        Self {
            is_valid: true,
            address: address.into(),
            legacy_type: Some(legacy_type),
        }
    }

    /// Returns whether the address is valid.
    pub fn is_valid(&self) -> bool {
        true
    }
}

/// Response for the Valid branch of the `z_validateaddress` RPC.
/// Note that the `ismine` field is present for `zcashd` but intentionally omitted here.
#[derive(Clone, Debug, PartialEq)]
pub enum AddressData {
    /// Transparent P2PKH.
    P2pkh {
        /// Common address fields.
        common: CommonFields,
    },

    /// Transparent P2SH
    P2sh {
        /// Common address fields
        common: CommonFields,
    },

    /// Sprout address type
    Sprout {
        /// Common address fields
        common: CommonFields,

        /// Hex of `a_pk`
        paying_key: Option<String>,

        /// The hex value of the transmission key, pk_enc
        transmission_key: Option<String>,
    },

    /// Sapling address type
    Sapling {
        /// Common address fields
        common: CommonFields,

        /// Hex of the diversifier `d`
        diversifier: Option<String>,

        /// Hex of `pk_d`
        diversified_transmission_key: Option<String>,
    },

    /// Unified Address (UA). `zcashd` currently returns no extra fields for UA.
    Unified {
        /// Common address fields
        common: CommonFields,
    },
}

impl AddressData {
    fn common(&self) -> &CommonFields {
        match self {
            AddressData::P2pkh { common, .. }
            | AddressData::P2sh { common, .. }
            | AddressData::Sprout { common, .. }
            | AddressData::Sapling { common, .. }
            | AddressData::Unified { common, .. } => common,
        }
    }

    fn variant_type(&self) -> ZValidateAddressType {
        match self {
            AddressData::P2pkh { .. } => ZValidateAddressType::P2pkh,
            AddressData::P2sh { .. } => ZValidateAddressType::P2sh,
            AddressData::Sprout { .. } => ZValidateAddressType::Sprout,
            AddressData::Sapling { .. } => ZValidateAddressType::Sapling,
            AddressData::Unified { .. } => ZValidateAddressType::Unified,
        }
    }
}

impl Serialize for AddressData {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let tag = self.variant_type();

        let mut map = s.serialize_map(None)?;
        // Mirror tags on output
        map.serialize_entry("address_type", &tag)?;
        map.serialize_entry("type", &tag)?;

        // Common
        let c = self.common();
        map.serialize_entry("isvalid", &true)?;
        map.serialize_entry("address", &c.address)?;

        // Different variants
        match self {
            AddressData::P2pkh { .. } | AddressData::P2sh { .. } => (),
            AddressData::Sprout {
                paying_key,
                transmission_key,
                ..
            } => {
                if let Some(pk) = paying_key {
                    map.serialize_entry("payingkey", pk)?;
                }
                if let Some(tk) = transmission_key {
                    map.serialize_entry("transmissionkey", tk)?;
                }
            }
            AddressData::Sapling {
                diversifier,
                diversified_transmission_key,
                ..
            } => {
                if let Some(d) = diversifier {
                    map.serialize_entry("diversifier", d)?;
                }
                if let Some(dtk) = diversified_transmission_key {
                    map.serialize_entry("diversifiedtransmissionkey", dtk)?;
                }
            }
            AddressData::Unified { .. } => (),
        }

        map.end()
    }
}

impl<'de> Deserialize<'de> for AddressData {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut v = Value::deserialize(d)?;
        let obj = v
            .as_object_mut()
            .ok_or_else(|| de::Error::custom("expected object"))?;

        let address_type: Option<String> = obj
            .get("address_type")
            .and_then(|x| x.as_str())
            .map(ToOwned::to_owned);
        let legacy_type: Option<String> = obj
            .get("type")
            .and_then(|x| x.as_str())
            .map(ToOwned::to_owned);

        let (chosen, needs_address_type, needs_legacy_type) =
            match (address_type.as_deref(), legacy_type.as_deref()) {
                (Some(a), Some(t)) if a != t => {
                    return Err(de::Error::custom("`type` must match `address_type`"));
                }
                (Some(a), Some(_)) => (a.to_string(), false, false),
                (Some(a), None) => (a.to_string(), false, true),
                (None, Some(t)) => (t.to_string(), true, false),
                (None, None) => return Err(de::Error::custom("missing `address_type` and `type`")),
            };

        if needs_address_type {
            obj.insert("address_type".into(), Value::String(chosen.clone()));
        }
        if needs_legacy_type {
            obj.insert("type".into(), Value::String(chosen.clone()));
        }

        let is_valid = obj
            .get("isvalid")
            .and_then(|b| b.as_bool())
            .ok_or_else(|| de::Error::custom("missing `isvalid`"))?;
        if !is_valid {
            return Err(de::Error::custom("valid branch must have isvalid=true"));
        }

        let address = obj
            .get("address")
            .and_then(|s| s.as_str())
            .ok_or_else(|| de::Error::custom("missing `address`"))?
            .to_owned();

        let tag = match chosen.as_str() {
            "p2pkh" => ZValidateAddressType::P2pkh,
            "p2sh" => ZValidateAddressType::P2sh,
            "sprout" => ZValidateAddressType::Sprout,
            "sapling" => ZValidateAddressType::Sapling,
            "unified" => ZValidateAddressType::Unified,
            other => {
                return Err(de::Error::unknown_variant(
                    other,
                    &["p2pkh", "p2sh", "sprout", "sapling", "unified"],
                ))
            }
        };

        let common = CommonFields {
            is_valid: true,
            address,
            legacy_type: Some(tag),
        };

        Ok(match tag {
            ZValidateAddressType::P2pkh => AddressData::P2pkh { common },
            ZValidateAddressType::P2sh => AddressData::P2sh { common },
            ZValidateAddressType::Sprout => {
                let paying_key = obj
                    .get("payingkey")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned);
                let transmission_key = obj
                    .get("transmissionkey")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned);
                AddressData::Sprout {
                    common,
                    paying_key,
                    transmission_key,
                }
            }
            ZValidateAddressType::Sapling => {
                let diversifier = obj
                    .get("diversifier")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned);
                let diversified_transmission_key = obj
                    .get("diversifiedtransmissionkey")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned);

                AddressData::Sapling {
                    common,
                    diversifier,
                    diversified_transmission_key,
                }
            }
            ZValidateAddressType::Unified => AddressData::Unified { common },
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verifies that a type can be serialized and deserialized with the same shape.
    ///
    /// If the type does not have the same shape after serialization and deserialization, this function will panic.
    fn roundtrip<T>(value: &T)
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + std::fmt::Debug + PartialEq,
    {
        let s = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&s).unwrap();
        assert_eq!(&back, value);
    }

    #[test]
    fn invalid_roundtrip_and_shape() {
        let invalid_response = ZValidateAddressResponse::invalid();
        roundtrip(&invalid_response);

        let json_value = serde_json::to_value(&invalid_response).unwrap();
        assert_eq!(json_value, json!({ "isvalid": false }));

        // Invalid must reject isvalid=true when deserialized directly
        let bad = r#"{ "isvalid": true }"#;
        let err = serde_json::from_str::<InvalidZValidateAddress>(bad).unwrap_err();
        assert!(err.to_string().contains("isvalid=false"));
    }

    #[test]
    fn valid_p2pkh_roundtrip_and_fields() {
        let valid =
            ValidZValidateAddress::p2pkh("t1abc").with_legacy_type(ZValidateAddressType::P2pkh);

        let top = ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(valid.clone()));
        roundtrip(&top);

        let json_value = serde_json::to_value(&top).unwrap();

        // Compare as Value so we don't care about field order
        assert_eq!(
            json_value,
            json!({
                "isvalid": true,
                "address": "t1abc",
                "type": "p2pkh",
                "address_type": "p2pkh",
            })
        );

        if let ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address(), "t1abc");
            assert_eq!(v.address_type(), ZValidateAddressType::P2pkh);
            assert_eq!(v.legacy_type(), Some(ZValidateAddressType::P2pkh));
            assert!(v.sprout_keys().is_none());
            assert!(v.sapling_keys().is_none());
        } else {
            panic!("expected valid p2pkh");
        }
    }

    #[test]
    fn valid_p2sh() {
        let valid = ValidZValidateAddress::p2sh("t3zzz");
        let top = ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(valid.clone()));
        roundtrip(&top);

        let json_value = serde_json::to_value(&top).unwrap();
        assert_eq!(
            json_value,
            json!({
                "isvalid": true,
                "address": "t3zzz",
                "address_type": "p2sh",
                "type": "p2sh",
            })
        );

        if let ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address_type(), ZValidateAddressType::P2sh);
        }
    }

    #[test]
    fn valid_sprout_roundtrip_and_fields() {
        let valid = ValidZValidateAddress::sprout("zc1qq", Some("apkhex"), Some("pkenc"));
        let top = ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(valid.clone()));
        roundtrip(&top);

        let json_value = serde_json::to_value(&top).unwrap();
        assert_eq!(
            json_value,
            json!({
                "isvalid": true,
                "address": "zc1qq",
                "address_type": "sprout",
                "type": "sprout",
                "payingkey": "apkhex",
                "transmissionkey": "pkenc",
            })
        );

        if let ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address_type(), ZValidateAddressType::Sprout);
            assert_eq!(v.sprout_keys(), Some(("apkhex", "pkenc")));
            assert!(v.sapling_keys().is_none());
        }
    }

    #[test]
    fn valid_sapling_roundtrip_and_fields() {
        let valid = ValidZValidateAddress::sapling("zs1xx", Some("dhex"), Some("pkdhex"))
            .with_legacy_type(ZValidateAddressType::Sapling);
        let top = ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(valid.clone()));
        roundtrip(&top);

        let json_value = serde_json::to_value(&top).unwrap();
        assert_eq!(
            json_value,
            json!({
                "isvalid": true,
                "address": "zs1xx",
                "type": "sapling",
                "address_type": "sapling",
                "diversifier": "dhex",
                "diversifiedtransmissionkey": "pkdhex",
            })
        );

        if let ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address_type(), ZValidateAddressType::Sapling);
            assert_eq!(v.sapling_keys(), Some(("dhex", "pkdhex")));
            assert!(v.sprout_keys().is_none());
        }
    }

    #[test]
    fn valid_unified_has_no_ismine_and_no_legacy_type() {
        let valid = ValidZValidateAddress::unified("u1blah");
        let top = ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(valid.clone()));
        roundtrip(&top);

        // Assert that "ismine" is absent
        let json_value = serde_json::to_value(&top).unwrap();
        assert_eq!(
            json_value,
            json!({
                "isvalid": true,
                "address": "u1blah",
                "address_type": "unified",
                "type": "unified"
            })
        );

        if let ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address_type(), ZValidateAddressType::Unified);
            assert_eq!(v.legacy_type(), Some(ZValidateAddressType::Unified));
        }
    }

    #[test]
    fn invalid_branch_enforces_isvalid_false_no_other() {
        // This JSON looks like sapling but has isvalid=false, so it must fail for ValidZValidateAddress
        let bad = r#"
        {
            "isvalid": false,
            "address": "zs1bad",
            "address_type": "sapling",
            "diversifier": "aa",
            "diversifiedtransmissionkey": "bb"
        }"#;

        let err = serde_json::from_str::<ValidZValidateAddress>(bad).unwrap_err();
        assert!(err.to_string().contains("isvalid=true"));

        // It will also fail for the Invalid branch, as it must ONLY contain isvalid=false
        let bad_invalid = serde_json::from_str::<ValidZValidateAddress>(bad);
        assert!(bad_invalid.is_err());
    }

    #[test]
    fn missing_address_type_is_rejected_for_valid() {
        // Missing "address_type" means AddressData can't be chosen
        let bad = r#"{ "isvalid": true, "address": "zs1nope" }"#;
        let result = serde_json::from_str::<ValidZValidateAddress>(bad);
        assert!(result.is_err());
    }

    #[test]
    fn top_level_unknown_on_null() {
        // Untagged enum with a unit variant means `null` maps to `Unknown`.
        let null_value: ZValidateAddressResponse = serde_json::from_str("{}").unwrap();
        match null_value {
            ZValidateAddressResponse::Unknown(_) => {}
            _ => panic!("expected Unknown"),
        }

        // Serializing Unknown produces `null`.
        let null_serialized =
            serde_json::to_string(&ZValidateAddressResponse::Unknown(BTreeMap::new())).unwrap();
        assert_eq!(null_serialized, "{}");
    }

    #[test]
    fn ismine_state_json_behavior() {
        let valid_p2pkh = ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::p2pkh("t1omitted"),
        ));
        let json_value = serde_json::to_value(&valid_p2pkh).unwrap();
        assert_eq!(json_value.get("ismine"), None);

        // True/false encoded when set
        let v_true = ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::p2pkh("t1mine"),
        ));
        let v_false = ZValidateAddressResponse::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::p2pkh("t1not"),
        ));
        let j_true = serde_json::to_value(&v_true).unwrap();
        let j_false = serde_json::to_value(&v_false).unwrap();
        assert_eq!(j_true.get("ismine"), None);
        assert_eq!(j_false.get("ismine"), None);
    }

    #[test]
    fn helpers_return_expected_values() {
        let sapling_with_ismine =
            ValidZValidateAddress::sapling("zs1addr", Some("dhex"), Some("pkdhex"));
        assert_eq!(sapling_with_ismine.address(), "zs1addr");
        assert_eq!(
            sapling_with_ismine.address_type(),
            ZValidateAddressType::Sapling
        );
        assert_eq!(
            sapling_with_ismine.legacy_type(),
            Some(ZValidateAddressType::Sapling)
        );
        assert_eq!(sapling_with_ismine.sapling_keys(), Some(("dhex", "pkdhex")));
        assert!(sapling_with_ismine.sprout_keys().is_none());
    }
}
