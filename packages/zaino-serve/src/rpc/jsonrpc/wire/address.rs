//! Wire shapes for `validateaddress` and `z_validateaddress`.

use serde::{
    ser::{SerializeMap, SerializeStruct},
    Serialize, Serializer,
};
use zaino_address::{ValidatedAddress, ZValidatedAddress};
use zebra_rpc::client::ValidateAddressResponse;

/// Renders a [`ValidatedAddress`] into Zebra's `validateaddress` response.
///
/// Zebra already defines and serializes this shape correctly, so there is no
/// wire struct here — only the mapping. A free function rather than a method
/// because `ValidateAddressResponse` is foreign.
pub(crate) fn validate_address_from_domain(validated: ValidatedAddress) -> ValidateAddressResponse {
    match validated {
        ValidatedAddress::Invalid => ValidateAddressResponse::invalid(),
        ValidatedAddress::Transparent { address, is_script } => {
            ValidateAddressResponse::new(true, Some(address), Some(is_script))
        }
    }
}

/// The `z_validateaddress` response.
///
/// Zebra has no type for this legacy-only method, so the shape is defined here.
/// The serialization is hand-written because the legacy full node's is irregular in two ways a
/// derive cannot express: the address kind is emitted twice, under both
/// `address_type` and the legacy `type` key, and the per-kind key material
/// fields appear only for the kinds that have them.
///
/// the legacy full node's `ismine` field is deliberately absent: Zaino holds no wallet, so it
/// cannot answer the question, and emitting `false` would be a lie rather than
/// an omission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZValidateAddressWire {
    /// Serializes as `{"isvalid": false}` and nothing else.
    Invalid,

    /// Serializes with the common fields plus any key material for the kind.
    Valid(ValidZValidateAddressWire),
}

impl ZValidateAddressWire {
    /// Renders a [`ZValidatedAddress`] into the wire shape, hex-encoding the
    /// Sapling key material on the way out.
    pub fn from_domain(validated: ZValidatedAddress) -> Self {
        match validated {
            ZValidatedAddress::Invalid => Self::Invalid,
            ZValidatedAddress::P2pkh { address } => Self::Valid(ValidZValidateAddressWire {
                address,
                kind: AddressKindWire::P2pkh,
                keys: None,
            }),
            ZValidatedAddress::P2sh { address } => Self::Valid(ValidZValidateAddressWire {
                address,
                kind: AddressKindWire::P2sh,
                keys: None,
            }),
            ZValidatedAddress::Unified { address } => Self::Valid(ValidZValidateAddressWire {
                address,
                kind: AddressKindWire::Unified,
                keys: None,
            }),
            ZValidatedAddress::Sapling {
                address,
                diversifier,
                diversified_transmission_key,
            } => Self::Valid(ValidZValidateAddressWire {
                address,
                kind: AddressKindWire::Sapling,
                keys: Some(SaplingKeysWire {
                    diversifier: hex::encode(diversifier),
                    diversified_transmission_key: hex::encode(diversified_transmission_key),
                }),
            }),
        }
    }
}

impl Serialize for ZValidateAddressWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Invalid => {
                let mut invalid = serializer.serialize_struct("ZValidateAddressWire", 1)?;
                invalid.serialize_field("isvalid", &false)?;
                invalid.end()
            }
            Self::Valid(valid) => valid.serialize(serializer),
        }
    }
}

/// The `isvalid: true` branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidZValidateAddressWire {
    /// The address, as reported back to the caller.
    address: String,
    /// Which kind of address it is. Emitted under two keys; see
    /// [`ZValidateAddressWire`].
    kind: AddressKindWire,
    /// Key material, present only for Sapling.
    keys: Option<SaplingKeysWire>,
}

impl Serialize for ValidZValidateAddressWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;

        // the legacy full node emits the kind under both keys. `type` is the legacy name and
        // clients still read it, so both are mirrored from the one field.
        map.serialize_entry("address_type", &self.kind)?;
        map.serialize_entry("type", &self.kind)?;

        map.serialize_entry("isvalid", &true)?;
        map.serialize_entry("address", &self.address)?;

        if let Some(keys) = &self.keys {
            map.serialize_entry("diversifier", &keys.diversifier)?;
            map.serialize_entry(
                "diversifiedtransmissionkey",
                &keys.diversified_transmission_key,
            )?;
        }

        map.end()
    }
}

/// Hex-encoded Sapling address components.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SaplingKeysWire {
    diversifier: String,
    diversified_transmission_key: String,
}

/// Address kinds the legacy full node's `z_validateaddress` names.
///
/// There is no `sprout` variant: Zaino does not classify Sprout addresses, so
/// this can never be asked to serialize one. See
/// [`ZValidatedAddress`]'s Sprout note.
#[derive(Copy, Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AddressKindWire {
    P2pkh,
    P2sh,
    Sapling,
    Unified,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Field names and the `type` / `address_type` duplication are a wire
    /// contract; pin the exact JSON rather than round-tripping.
    #[test]
    fn invalid_serializes_to_isvalid_false_only() {
        let wire = ZValidateAddressWire::from_domain(ZValidatedAddress::Invalid);
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({ "isvalid": false })
        );
    }

    #[test]
    fn p2pkh_shape() {
        let wire = ZValidateAddressWire::from_domain(ZValidatedAddress::P2pkh {
            address: "t1abc".into(),
        });
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({
                "isvalid": true,
                "address": "t1abc",
                "type": "p2pkh",
                "address_type": "p2pkh",
            })
        );
    }

    #[test]
    fn p2sh_shape() {
        let wire = ZValidateAddressWire::from_domain(ZValidatedAddress::P2sh {
            address: "t3zzz".into(),
        });
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({
                "isvalid": true,
                "address": "t3zzz",
                "type": "p2sh",
                "address_type": "p2sh",
            })
        );
    }

    /// Sapling is the only kind carrying key material, and the hex encoding
    /// happens here rather than in the domain type.
    #[test]
    fn sapling_shape_hex_encodes_key_material() {
        let wire = ZValidateAddressWire::from_domain(ZValidatedAddress::Sapling {
            address: "zs1xx".into(),
            diversifier: [0xab; 11],
            diversified_transmission_key: [0xcd; 32],
        });
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({
                "isvalid": true,
                "address": "zs1xx",
                "type": "sapling",
                "address_type": "sapling",
                "diversifier": "ab".repeat(11),
                "diversifiedtransmissionkey": "cd".repeat(32),
            })
        );
    }

    #[test]
    fn unified_shape_has_no_key_material() {
        let wire = ZValidateAddressWire::from_domain(ZValidatedAddress::Unified {
            address: "u1blah".into(),
        });
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({
                "isvalid": true,
                "address": "u1blah",
                "type": "unified",
                "address_type": "unified",
            })
        );
    }

    /// Zaino holds no wallet, so `ismine` must never appear.
    #[test]
    fn ismine_is_never_emitted() {
        for domain in [
            ZValidatedAddress::Invalid,
            ZValidatedAddress::P2pkh {
                address: "t1abc".into(),
            },
            ZValidatedAddress::Unified {
                address: "u1blah".into(),
            },
        ] {
            let value = serde_json::to_value(ZValidateAddressWire::from_domain(domain)).unwrap();
            assert_eq!(value.get("ismine"), None);
        }
    }

    #[test]
    fn validate_address_maps_transparent_and_invalid() {
        let script = validate_address_from_domain(ValidatedAddress::Transparent {
            address: "t3zzz".into(),
            is_script: true,
        });
        assert!(script.is_valid());
        assert_eq!(script.address().as_deref(), Some("t3zzz"));
        assert_eq!(script.is_script(), Some(true));

        let invalid = validate_address_from_domain(ValidatedAddress::Invalid);
        assert!(!invalid.is_valid());
        assert_eq!(invalid.address().as_deref(), None);
    }
}

/// End-to-end vectors: address string in, served JSON out.
///
/// `z_validateaddress` is the composition of [`zaino_address::z_validate_address`]
/// with [`ZValidateAddressWire::from_domain`] and nothing else, so the whole
/// classification contract is pinned here. The live suite keeps one vector, to
/// prove the endpoint reaches this pair.
#[cfg(test)]
mod served_vectors {
    use super::*;
    use serde_json::{json, Value};
    use zebra_chain::parameters::Network;

    // Captured from zcashd's `z_validateaddress`.
    const P2PKH: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
    const P2SH: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";
    const SAPLING: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
    const SAPLING_DIVERSIFIER: &str = "977e0b930ee6c11e4d26f8";
    const SAPLING_DIVERSIFIED_TRANSMISSION_KEY: &str =
        "553ef2f328096a7c2aac6dec85b76b6b9243e733dc9db2eacce3eb8c60592c88";
    const UNIFIED: &str = "uregtest1njwg60x0jarhyuuxrcdvw854p68cgdfe85822lmclc7z9vy9xqr7t49n3d97k2dwlee82skwwe0ens0rc06p4vr04tvd3j9ckl3qry83ckay4l4ngdq9atg7vuj9z58tfjs0mnsgyrnprtqfv8almu564z498zy6tp2aa569tk8fyhdazyhytel2m32awe4kuy6qq996um3ljaajj36";
    const SPROUT: &str = "ztfhKyLouqi8sSwjRm4YMQdWPjTmrJ4QgtziVQ1Kd1e9EsRHYKofjoJdF438FwcUQnix8yrbSrzPpJJNABewgNffs5d4YZJ";

    fn served(address: &str, network: &Network) -> Value {
        serde_json::to_value(ZValidateAddressWire::from_domain(
            zaino_address::z_validate_address(address.to_string(), network),
        ))
        .expect("ZValidateAddressWire serializes infallibly")
    }

    /// The kind is what a classifier bug would get wrong, and it is emitted
    /// under both keys, so both are pinned for every kind.
    #[test]
    fn each_kind_serves_its_own_shape() {
        let network = Network::new_regtest(Default::default());

        for (address, expected) in [
            (
                P2PKH,
                json!({
                    "isvalid": true,
                    "address": P2PKH,
                    "type": "p2pkh",
                    "address_type": "p2pkh",
                }),
            ),
            (
                P2SH,
                json!({
                    "isvalid": true,
                    "address": P2SH,
                    "type": "p2sh",
                    "address_type": "p2sh",
                }),
            ),
            (
                SAPLING,
                json!({
                    "isvalid": true,
                    "address": SAPLING,
                    "type": "sapling",
                    "address_type": "sapling",
                    "diversifier": SAPLING_DIVERSIFIER,
                    "diversifiedtransmissionkey": SAPLING_DIVERSIFIED_TRANSMISSION_KEY,
                }),
            ),
            (
                UNIFIED,
                json!({
                    "isvalid": true,
                    "address": UNIFIED,
                    "type": "unified",
                    "address_type": "unified",
                }),
            ),
        ] {
            assert_eq!(served(address, &network), expected, "{address}");
        }
    }

    /// Sprout is here because Zaino declines to classify it rather than
    /// failing to parse it — the only thing to assert about it is "invalid".
    #[test]
    fn unclassifiable_addresses_serve_isvalid_false_alone() {
        let network = Network::new_regtest(Default::default());

        for address in [
            SPROUT,
            "t1123456789ABCDEFGHJKLMNPQRSTUVWXY",
            "t1000000000000000000000000000000000",
            "not an address",
        ] {
            assert_eq!(
                served(address, &network),
                json!({ "isvalid": false }),
                "{address}"
            );
        }
    }

    /// An address well-formed for another network must not validate; the
    /// regtest vectors are rejected on mainnet and vice versa.
    #[test]
    fn network_scoping_rejects_foreign_addresses() {
        let mainnet = Network::Mainnet;

        for address in [P2PKH, P2SH, SAPLING, UNIFIED] {
            assert_eq!(
                served(address, &mainnet),
                json!({ "isvalid": false }),
                "{address} is not a mainnet address"
            );
        }
    }
}
