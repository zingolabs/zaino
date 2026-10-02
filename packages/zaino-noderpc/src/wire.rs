//! Wire <-> domain conversion, owned by the adapter.
//!
//! Both directions live here: `*_from_hex` is the fallible external-input
//! validation (wire -> domain), and `to_hex` / `spend_status_to_wire` are the
//! domain -> wire renderings. No domain crate depends on any wire schema.

pub mod params;
pub mod response;

use zaino_address::{ValidatedAddress, ZValidatedAddress};
use zaino_primitives::types::AddressDelta;
use zaino_primitives::types::TransactionId;
use zaino_service::SpendStatus;

use crate::error::RpcError;
use crate::wire::response::{AddressDeltaEntry, ValidateAddressResponse, ZValidateAddressResponse};

fn hex_val(c: u8) -> Result<u8, RpcError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(RpcError::InvalidParams(format!(
            "invalid hex digit: {c:#x}"
        ))),
    }
}

/// Decode a hex string to bytes (wire -> domain input validation).
pub(crate) fn bytes_from_hex(s: &str) -> Result<Vec<u8>, RpcError> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(RpcError::InvalidParams("odd-length hex".into()));
    }
    bytes
        .chunks_exact(2)
        .map(|pair| Ok((hex_val(pair[0])? << 4) | hex_val(pair[1])?))
        .collect()
}

/// Decode a 32-byte transaction id (wire -> domain input validation).
pub(crate) fn txid_from_hex(s: &str) -> Result<TransactionId, RpcError> {
    let arr: [u8; 32] = bytes_from_hex(s)?
        .try_into()
        .map_err(|_| RpcError::InvalidParams("txid must be 32 bytes".into()))?;
    Ok(TransactionId::from(arr))
}

/// Lowercase hex of an arbitrary-length byte payload (domain -> wire).
pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex of a 32-byte hash — a hash, block id, or txid (domain -> wire).
/// The fixed-length signature documents the hash case at its call sites.
pub(crate) fn to_hex(bytes: [u8; 32]) -> String {
    bytes_to_hex(&bytes)
}

/// Render an address delta for the wire (domain -> wire).
pub(crate) fn delta_to_wire(delta: AddressDelta) -> AddressDeltaEntry {
    AddressDeltaEntry {
        satoshis: delta.satoshis.as_i64(),
        txid: to_hex(delta.txid.into()),
        index: delta.index,
        block_index: delta.block_index,
        height: delta.height.into(),
        address: delta.address.as_str().to_owned(),
    }
}

/// Render a spend status as a wire string (domain -> wire). Exhaustive by
/// design — a new `SpendStatus` variant should force a decision here.
pub(crate) fn spend_status_to_wire(status: SpendStatus) -> String {
    match status {
        SpendStatus::Unspent => "unspent".to_string(),
        SpendStatus::Spent { by } => format!("spent:{}", to_hex(by.into())),
        SpendStatus::SpentSpenderUnknown => "spent".to_string(),
        SpendStatus::NoSuchOutput => "none".to_string(),
    }
}

/// Render a transparent-address validation for the wire (domain -> wire).
/// Exhaustive by design — a new variant should force a decision here.
pub(crate) fn validated_to_wire(validated: ValidatedAddress) -> ValidateAddressResponse {
    match validated {
        ValidatedAddress::Invalid => ValidateAddressResponse {
            isvalid: false,
            address: None,
            isscript: None,
        },
        ValidatedAddress::Transparent { address, is_script } => ValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            isscript: Some(is_script),
        },
    }
}

/// Render a shielded-aware address validation for the wire (domain -> wire).
/// Exhaustive by design — a new variant should force a decision here. The
/// Sapling key material renders through `bytes_to_hex` in zcashd's big-endian
/// order (see [`zaino_address::sapling_key_bytes`]); a unified address reports
/// no components, matching zcashd.
pub(crate) fn z_validated_to_wire(validated: ZValidatedAddress) -> ZValidateAddressResponse {
    match validated {
        ZValidatedAddress::Invalid => ZValidateAddressResponse {
            isvalid: false,
            address: None,
            address_type: None,
            diversifier: None,
            diversified_transmission_key: None,
        },
        ZValidatedAddress::P2pkh { address } => ZValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            address_type: Some("p2pkh".to_string()),
            diversifier: None,
            diversified_transmission_key: None,
        },
        ZValidatedAddress::P2sh { address } => ZValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            address_type: Some("p2sh".to_string()),
            diversifier: None,
            diversified_transmission_key: None,
        },
        ZValidatedAddress::Sapling {
            address,
            diversifier,
            diversified_transmission_key,
        } => ZValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            address_type: Some("sapling".to_string()),
            diversifier: Some(bytes_to_hex(&diversifier)),
            diversified_transmission_key: Some(bytes_to_hex(&diversified_transmission_key)),
        },
        ZValidatedAddress::Unified { address } => ZValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            address_type: Some("unified".to_string()),
            diversifier: None,
            diversified_transmission_key: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{validated_to_wire, z_validated_to_wire};
    use zaino_address::{ValidatedAddress, ZValidatedAddress};

    /// A script-hash transparent address renders `isscript: true` with the
    /// address echoed. Fails if the `is_script` flag is dropped or inverted.
    #[test]
    fn transparent_script_hash_renders_isscript_true() {
        let got = validated_to_wire(ValidatedAddress::Transparent {
            address: "t3script".to_string(),
            is_script: true,
        });
        assert!(got.isvalid);
        assert_eq!(got.address.as_deref(), Some("t3script"));
        assert_eq!(got.isscript, Some(true));
    }

    /// Sapling renders its key material through `bytes_to_hex` in the order the
    /// domain supplies it — zcashd's big-endian order, not reversed. The two
    /// arrays are ascending, so a reversal would change every hex string; this
    /// fails if the bytes are reordered. It also pins the `sapling` tag and the
    /// `diversifiedtransmissionkey` wire field.
    #[test]
    fn sapling_renders_key_material_as_is() {
        let diversifier: [u8; 11] = core::array::from_fn(|i| u8::try_from(i).expect("fits u8"));
        let key: [u8; 32] = core::array::from_fn(|i| u8::try_from(i).expect("fits u8"));
        let got = z_validated_to_wire(ZValidatedAddress::Sapling {
            address: "zs1sapling".to_string(),
            diversifier,
            diversified_transmission_key: key,
        });
        assert!(got.isvalid);
        assert_eq!(got.address.as_deref(), Some("zs1sapling"));
        assert_eq!(got.address_type.as_deref(), Some("sapling"));
        assert_eq!(got.diversifier.as_deref(), Some("000102030405060708090a"));
        assert_eq!(
            got.diversified_transmission_key.as_deref(),
            Some("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
        );
    }

    /// A unified address carries only its address — zcashd reports no
    /// components, so the component fields stay `None`. Fails if the conversion
    /// ever synthesises a diversifier or key for a unified address.
    #[test]
    fn unified_reports_no_components() {
        let got = z_validated_to_wire(ZValidatedAddress::Unified {
            address: "u1unified".to_string(),
        });
        assert!(got.isvalid);
        assert_eq!(got.address_type.as_deref(), Some("unified"));
        assert!(got.diversifier.is_none());
        assert!(got.diversified_transmission_key.is_none());
    }

    /// The two serde renames are the only thing aligning Rust field names with
    /// zcashd's: `address_type` must serialize as `address_type`, and
    /// `diversified_transmission_key` as `diversifiedtransmissionkey`. Also pins
    /// that a `None` component is omitted rather than serialized as `null`.
    #[test]
    fn sapling_response_serializes_zcashd_field_names() {
        let json = serde_json::to_value(z_validated_to_wire(ZValidatedAddress::Sapling {
            address: "zs1sapling".to_string(),
            diversifier: [0u8; 11],
            diversified_transmission_key: [0u8; 32],
        }))
        .expect("serialize");
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("address_type").and_then(|v| v.as_str()),
            Some("sapling")
        );
        assert!(obj.contains_key("diversifiedtransmissionkey"));
        assert!(
            !obj.contains_key("diversified_transmission_key"),
            "the Rust field name must not leak onto the wire"
        );

        // A unified response has no components: those keys are omitted, not null.
        let unified = serde_json::to_value(z_validated_to_wire(ZValidatedAddress::Unified {
            address: "u1unified".to_string(),
        }))
        .expect("serialize");
        let unified = unified.as_object().expect("a JSON object");
        assert!(!unified.contains_key("diversifier"));
        assert!(!unified.contains_key("diversifiedtransmissionkey"));
    }
}
