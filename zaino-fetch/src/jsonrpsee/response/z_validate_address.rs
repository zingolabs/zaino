//! Types associated with the `z_validateaddress` RPC request.

use std::convert::Infallible;

use serde::{
    de,
    ser::{SerializeMap, SerializeStruct},
    Deserialize, Deserializer, Serialize, Serializer,
};
use serde_json::Value;

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

impl ZValidateAddress {
    /// Constructs an unknown response.
    pub fn unknown() -> Self {
        ZValidateAddress::Unknown
    }

    /// Constructs an invalid response.
    pub fn invalid() -> Self {
        ZValidateAddress::Known(KnownZValidateAddress::Invalid(
            InvalidZValidateAddress::new(),
        ))
    }

    /// Constructs a valid response for a P2PKH address.
    pub fn p2pkh(address: impl Into<String>) -> Self {
        ZValidateAddress::Known(KnownZValidateAddress::Valid(ValidZValidateAddress::p2pkh(
            address,
        )))
    }

    /// Constructs a valid response for a P2SH address.
    pub fn p2sh(address: impl Into<String>) -> Self {
        ZValidateAddress::Known(KnownZValidateAddress::Valid(ValidZValidateAddress::p2sh(
            address,
        )))
    }

    /// Constructs a valid response for a Sapling address.
    pub fn sapling(
        address: impl Into<String>,
        diversifier: impl Into<String>,
        diversified_transmission_key: impl Into<String>,
    ) -> Self {
        ZValidateAddress::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::sapling(address, diversifier, diversified_transmission_key),
        ))
    }

    /// Constructs a valid response for a Sprout address.
    pub fn sprout(
        address: impl Into<String>,
        paying_key: impl Into<String>,
        transmission_key: impl Into<String>,
    ) -> Self {
        ZValidateAddress::Known(KnownZValidateAddress::Valid(ValidZValidateAddress::sprout(
            address,
            paying_key,
            transmission_key,
        )))
    }

    /// Constructs a valid response for a Unified address.
    pub fn unified(address: impl Into<String>) -> Self {
        ZValidateAddress::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::unified(address),
        ))
    }
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

/// The "invalid" shape is just `{ "isvalid": false }`.
/// Represent it as a unit-like struct so you *cannot* construct a "true" state.
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
pub struct ValidZValidateAddress(pub AddressData);

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
            common: CommonFields::valid(address, ZValidateAddressType::P2pkh),
            is_mine: IsMine::NotMine,
        })
    }

    /// Creates a response for a P2SH address.
    pub fn p2sh(address: impl Into<String>) -> Self {
        Self(AddressData::P2sh {
            common: CommonFields::valid(address, ZValidateAddressType::P2sh),
            is_mine: IsMine::NotMine,
        })
    }

    /// Creates a response for a Sprout address.
    pub fn sprout(
        address: impl Into<String>,
        paying_key: impl Into<String>,
        transmission_key: impl Into<String>,
    ) -> Self {
        Self(AddressData::Sprout {
            common: CommonFields::valid(address, ZValidateAddressType::Sprout),
            paying_key: paying_key.into(),
            transmission_key: transmission_key.into(),
            is_mine: IsMine::NotMine,
        })
    }

    /// Creates a response for a Sapling address.
    pub fn sapling(
        address: impl Into<String>,
        diversifier: impl Into<String>,
        diversified_transmission_key: impl Into<String>,
    ) -> Self {
        Self(AddressData::Sapling {
            common: CommonFields::valid(address, ZValidateAddressType::Sapling),
            diversifier: diversifier.into(),
            diversified_transmission_key: diversified_transmission_key.into(),
            is_mine: IsMine::NotMine,
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
}

/// Common fields that appear for all valid responses.
#[derive(Clone, Debug, PartialEq)]
pub struct CommonFields {
    is_valid: bool,

    /// The address original provided.
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

/// `ismine` wrapper. Originally used by `zcashd`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "Option<bool>", into = "Option<bool>")]
#[derive(Default)]
pub enum IsMine {
    /// The address is in the wallet.
    Mine,

    /// The address is not in the wallet.
    NotMine,

    /// Unknown.
    #[default]
    Unknown,
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
#[derive(Clone, Debug, PartialEq)]
pub enum AddressData {
    /// Transparent P2PKH.
    P2pkh {
        /// Common address fields.
        common: CommonFields,

        /// Whether the address is in the wallet or not.
        is_mine: IsMine,
    },

    /// Transparent P2SH
    P2sh {
        /// Common address fields
        common: CommonFields,

        /// Whether the address is in the wallet or not.
        is_mine: IsMine,
    },

    /// Sprout address type
    Sprout {
        /// Common address fields
        common: CommonFields,

        /// Hex of `a_pk`
        paying_key: String,

        /// The hex value of the transmission key, pk_enc
        transmission_key: String,

        /// Whether the address is in the wallet or not.
        is_mine: IsMine,
    },

    /// Sapling address type
    Sapling {
        /// Common address fields
        common: CommonFields,

        /// Hex of the diversifier `d`
        diversifier: String,

        /// Hex of `pk_d`
        diversified_transmission_key: String,

        /// Whether the address is in the wallet or not.
        is_mine: IsMine,
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
            AddressData::P2pkh { is_mine, .. } | AddressData::P2sh { is_mine, .. } => {
                if let Some(b) = Option::<bool>::from(is_mine.clone()) {
                    map.serialize_entry("ismine", &b)?;
                }
            }
            AddressData::Sprout {
                paying_key,
                transmission_key,
                is_mine,
                ..
            } => {
                map.serialize_entry("payingkey", paying_key)?;
                map.serialize_entry("transmissionkey", transmission_key)?;
                if let Some(b) = Option::<bool>::from(is_mine.clone()) {
                    map.serialize_entry("ismine", &b)?;
                }
            }
            AddressData::Sapling {
                diversifier,
                diversified_transmission_key,
                is_mine,
                ..
            } => {
                map.serialize_entry("diversifier", diversifier)?;
                map.serialize_entry("diversifiedtransmissionkey", diversified_transmission_key)?;
                if let Some(b) = Option::<bool>::from(is_mine.clone()) {
                    map.serialize_entry("ismine", &b)?;
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

        let is_mine = IsMine::from(obj.get("ismine").and_then(|b| b.as_bool()));

        Ok(match tag {
            ZValidateAddressType::P2pkh => AddressData::P2pkh { common, is_mine },
            ZValidateAddressType::P2sh => AddressData::P2sh { common, is_mine },
            ZValidateAddressType::Sprout => {
                let paying_key = obj
                    .get("payingkey")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| de::Error::custom("missing `payingkey`"))?
                    .to_owned();
                let transmission_key = obj
                    .get("transmissionkey")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| de::Error::custom("missing `transmissionkey`"))?
                    .to_owned();
                AddressData::Sprout {
                    common,
                    paying_key,
                    transmission_key,
                    is_mine,
                }
            }
            ZValidateAddressType::Sapling => {
                let diversifier = obj
                    .get("diversifier")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| de::Error::custom("missing `diversifier`"))?
                    .to_owned();
                let diversified_transmission_key = obj
                    .get("diversifiedtransmissionkey")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| de::Error::custom("missing `diversifiedtransmissionkey`"))?
                    .to_owned();
                AddressData::Sapling {
                    common,
                    diversifier,
                    diversified_transmission_key,
                    is_mine,
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
    use serde_json::{json, Value};

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
        let v = ZValidateAddress::Known(KnownZValidateAddress::Invalid(
            InvalidZValidateAddress::new(),
        ));
        roundtrip(&v);

        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j, json!({ "isvalid": false }));

        // Invalid must reject isvalid=true when deserialized directly
        let bad = r#"{ "isvalid": true }"#;
        let err = serde_json::from_str::<InvalidZValidateAddress>(bad).unwrap_err();
        assert!(err.to_string().contains("isvalid=false"));
    }

    #[test]
    fn valid_p2pkh_roundtrip_and_fields() {
        let valid = ValidZValidateAddress::p2pkh("t1abc")
            .with_is_mine(IsMine::Mine)
            .with_legacy_type(ZValidateAddressType::P2pkh);

        let top = ZValidateAddress::Known(KnownZValidateAddress::Valid(valid.clone()));
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
                "ismine": true
            })
        );

        if let ZValidateAddress::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address(), "t1abc");
            assert_eq!(v.address_type(), ZValidateAddressType::P2pkh);
            assert_eq!(v.legacy_type(), Some(ZValidateAddressType::P2pkh));
            assert_eq!(v.is_mine(), IsMine::Mine);
            assert!(v.sprout_keys().is_none());
            assert!(v.sapling_keys().is_none());
        } else {
            panic!("expected valid p2pkh");
        }
    }

    #[test]
    fn valid_p2sh_with_notmine() {
        let valid = ValidZValidateAddress::p2sh("t3zzz").with_is_mine(IsMine::NotMine);
        let top = ZValidateAddress::Known(KnownZValidateAddress::Valid(valid.clone()));
        roundtrip(&top);

        let json_value = serde_json::to_value(&top).unwrap();
        assert_eq!(
            json_value,
            json!({
                "isvalid": true,
                "address": "t3zzz",
                "address_type": "p2sh",
                "type": "p2sh",
                "ismine": false
            })
        );

        if let ZValidateAddress::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address_type(), ZValidateAddressType::P2sh);
            assert_eq!(v.is_mine(), IsMine::NotMine);
        }
    }

    #[test]
    fn valid_sprout_roundtrip_and_fields() {
        let valid =
            ValidZValidateAddress::sprout("zc1qq", "apkhex", "pkenc").with_is_mine(IsMine::Mine);
        let top = ZValidateAddress::Known(KnownZValidateAddress::Valid(valid.clone()));
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
                "ismine": true
            })
        );

        if let ZValidateAddress::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address_type(), ZValidateAddressType::Sprout);
            assert_eq!(v.is_mine(), IsMine::Mine);
            assert_eq!(v.sprout_keys(), Some(("apkhex", "pkenc")));
            assert!(v.sapling_keys().is_none());
        }
    }

    #[test]
    fn valid_sapling_roundtrip_and_fields() {
        let valid = ValidZValidateAddress::sapling("zs1xx", "dhex", "pkdhex")
            .with_is_mine(IsMine::NotMine)
            .with_legacy_type(ZValidateAddressType::Sapling);
        let top = ZValidateAddress::Known(KnownZValidateAddress::Valid(valid.clone()));
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
                "ismine": false
            })
        );

        if let ZValidateAddress::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address_type(), ZValidateAddressType::Sapling);
            assert_eq!(v.is_mine(), IsMine::NotMine);
            assert_eq!(v.sapling_keys(), Some(("dhex", "pkdhex")));
            assert!(v.sprout_keys().is_none());
        }
    }

    #[test]
    fn valid_unified_has_no_ismine_and_no_legacy_type() {
        let valid = ValidZValidateAddress::unified("u1blah");
        let top = ZValidateAddress::Known(KnownZValidateAddress::Valid(valid.clone()));
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

        if let ZValidateAddress::Known(KnownZValidateAddress::Valid(v)) = top {
            assert_eq!(v.address_type(), ZValidateAddressType::Unified);
            assert_eq!(v.is_mine(), IsMine::Unknown);
            assert_eq!(v.legacy_type(), Some(ZValidateAddressType::Unified));
        }
    }

    #[test]
    fn valid_branch_enforces_isvalid_true() {
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

        // However, as a KnownZValidateAddress the same JSON should deserialize
        // into the Invalid branch (since our Invalid only checks `isvalid`).
        let ok: KnownZValidateAddress = serde_json::from_str(bad).unwrap();
        match ok {
            KnownZValidateAddress::Invalid(InvalidZValidateAddress { .. }) => {}
            _ => panic!("expected Invalid branch"),
        }
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
        let v: ZValidateAddress = serde_json::from_str("null").unwrap();
        match v {
            ZValidateAddress::Unknown => {}
            _ => panic!("expected Unknown"),
        }

        // Serializing Unknown produces `null`.
        let s = serde_json::to_string(&ZValidateAddress::Unknown).unwrap();
        assert_eq!(s, "null");
    }

    #[test]
    fn ismine_tri_state_json_behavior() {
        let v = ZValidateAddress::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::p2pkh("t1omitted"),
        ));
        let json_value = serde_json::to_value(&v).unwrap();
        assert_eq!(json_value.get("ismine"), Some(&Value::Bool(false)));

        // True/false encoded when set
        let v_true = ZValidateAddress::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::p2pkh("t1mine").with_is_mine(IsMine::Mine),
        ));
        let v_false = ZValidateAddress::Known(KnownZValidateAddress::Valid(
            ValidZValidateAddress::p2pkh("t1not").with_is_mine(IsMine::NotMine),
        ));
        let j_true = serde_json::to_value(&v_true).unwrap();
        let j_false = serde_json::to_value(&v_false).unwrap();
        assert_eq!(j_true.get("ismine"), Some(&Value::Bool(true)));
        assert_eq!(j_false.get("ismine"), Some(&Value::Bool(false)));
    }

    #[test]
    fn helpers_return_expected_values() {
        let v =
            ValidZValidateAddress::sapling("zs1addr", "dhex", "pkdhex").with_is_mine(IsMine::Mine);
        assert_eq!(v.address(), "zs1addr");
        assert_eq!(v.address_type(), ZValidateAddressType::Sapling);
        assert_eq!(v.legacy_type(), Some(ZValidateAddressType::Sapling));
        assert_eq!(v.is_mine(), IsMine::Mine);
        assert_eq!(v.sapling_keys(), Some(("dhex", "pkdhex")));
        assert!(v.sprout_keys().is_none());
    }
}
