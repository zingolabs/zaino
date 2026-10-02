//! Wire <-> domain conversion, owned by the adapter.
//!
//! Both directions live here: `*_from_hex` is the fallible external-input
//! validation (wire -> domain), and `to_hex` / the `*_to_wire` functions are the
//! domain -> wire renderings. No domain crate depends on any wire schema.

pub mod params;
pub mod response;

use zaino_address::{UnifiedReceivers, ValidatedAddress, ZValidatedAddress};
use zaino_primitives::types::AddressBalance;
use zaino_primitives::types::AddressDelta;
use zaino_primitives::types::TransactionId;
use zaino_primitives::types::{
    BlockchainInfo, NetworkUpgradeInfo, NetworkUpgradeStatus, ValuePoolBalance,
};

use crate::error::RpcError;
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltaEntry, BlockchainInfoResponse, NetworkUpgradeResponse,
    TipConsensusResponse, UnifiedReceiversResponse, ValidateAddressResponse, ValuePoolResponse,
    ZValidateAddressResponse,
};

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

/// Render a summed transparent balance for the wire (domain -> wire). The held
/// balance is supply-bounded and fits `u64`; lifetime receipts are a flow total
/// that is not supply-bounded and so render from `u128`.
pub(crate) fn address_balance_to_wire(total: AddressBalance) -> AddressBalanceResponse {
    AddressBalanceResponse {
        balance: total.balance.as_u64(),
        received: u128::from(total.received),
    }
}

/// Render a unified address's receivers for the wire (domain -> wire). Each
/// receiver kind is carried only when the address bundles one.
pub(crate) fn unified_receivers_to_wire(receivers: UnifiedReceivers) -> UnifiedReceiversResponse {
    UnifiedReceiversResponse {
        orchard: receivers.orchard,
        sapling: receivers.sapling,
        p2pkh: receivers.p2pkh,
        p2sh: receivers.p2sh,
    }
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

/// Render one value pool for the wire (domain -> wire). Only the exact zatoshi
/// integer is carried; the ZEC-denominated float zcashd also reports is dropped.
/// An empty id (the unnamed chain-supply total) is omitted by the response type.
fn value_pool_to_wire(pool: &ValuePoolBalance) -> ValuePoolResponse {
    ValuePoolResponse {
        id: pool.id.clone(),
        monitored: pool.monitored,
        chain_value_zat: pool.chain_value.as_u64(),
        value_delta_zat: pool.value_delta.map(|delta| delta.as_i64()),
    }
}

/// Render a network upgrade's status (domain -> wire) in zcashd's lowercase
/// vocabulary. Exhaustive by design — a new status should force a decision here.
fn upgrade_status_to_wire(status: NetworkUpgradeStatus) -> String {
    match status {
        NetworkUpgradeStatus::Active => "active".to_string(),
        NetworkUpgradeStatus::Pending => "pending".to_string(),
        NetworkUpgradeStatus::Disabled => "disabled".to_string(),
    }
}

/// Render one network upgrade for the wire (domain -> wire), returning the
/// consensus-branch-id key it is filed under and its body. The branch id is the
/// map key in zcashd's layout, written as 8-digit lowercase hex.
fn upgrade_to_wire(upgrade: &NetworkUpgradeInfo) -> (String, NetworkUpgradeResponse) {
    (
        upgrade.branch_id.to_string(),
        NetworkUpgradeResponse {
            name: upgrade.name.clone(),
            activation_height: upgrade.activation_height.into(),
            status: upgrade_status_to_wire(upgrade.status),
        },
    )
}

/// Render the validator's chain-info aggregate as the `getblockchaininfo`
/// response (domain -> wire).
///
/// Cumulative work renders as 64-character big-endian hex when the validator
/// tracks it, and is omitted otherwise — zero is not a possible amount of work,
/// so absence is the honest wire form rather than a zero a consumer could
/// compare.
pub(crate) fn blockchain_info_to_wire(info: BlockchainInfo) -> BlockchainInfoResponse {
    BlockchainInfoResponse {
        chain: info.chain,
        blocks: info.blocks.into(),
        headers: info.headers.into(),
        best_block_hash: to_hex(info.best_block_hash.into()),
        difficulty: info.difficulty,
        verification_progress: info.verification_progress,
        chain_work: info
            .chain_work
            .map(|work| bytes_to_hex(&work.to_be_bytes())),
        pruned: info.pruned,
        size_on_disk: info.size_on_disk,
        commitments: info.commitments,
        estimated_height: info.estimated_height.into(),
        chain_supply: value_pool_to_wire(&info.chain_supply),
        value_pools: info.value_pools.iter().map(value_pool_to_wire).collect(),
        upgrades: info.upgrades.iter().map(upgrade_to_wire).collect(),
        consensus: TipConsensusResponse {
            chaintip: info.consensus.chain_tip.to_string(),
            nextblock: info.consensus.next_block.to_string(),
        },
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
    use super::{blockchain_info_to_wire, validated_to_wire, z_validated_to_wire};
    use serde_json::Value;
    use zaino_address::{ValidatedAddress, ZValidatedAddress};
    use zaino_primitives::types::{
        AbsoluteChainWork, BlockHash, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds,
        Height, NetworkUpgradeInfo, NetworkUpgradeStatus, SignedZatoshis, ValuePoolBalance,
        Zatoshis,
    };

    /// A chain-info aggregate with a distinguishable, non-zero value in every
    /// field, so a golden assertion over it fails if any field is dropped,
    /// defaulted, or mis-mapped. Chainwork is `…deadbeef`, each height differs,
    /// `pruned` is the non-default `true`, and the two value pools exercise the
    /// named / unnamed and present / absent-delta cases.
    fn scripted_info() -> BlockchainInfo {
        let mut work_bytes = [0u8; 32];
        work_bytes[28..].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        BlockchainInfo {
            chain: "main".to_string(),
            blocks: Height::try_from(800_001).expect("valid height"),
            headers: Height::try_from(800_002).expect("valid height"),
            estimated_height: Height::try_from(800_003).expect("valid height"),
            best_block_hash: BlockHash::from([0x11u8; 32]),
            difficulty: 123.5,
            verification_progress: 0.75,
            chain_work: AbsoluteChainWork::try_from_reported(work_bytes).expect("in-range work"),
            pruned: true,
            size_on_disk: 4_096,
            commitments: 7,
            chain_supply: ValuePoolBalance {
                id: String::new(),
                chain_value: Zatoshis::new(21_000_000).expect("valid amount"),
                monitored: true,
                value_delta: None,
            },
            value_pools: vec![ValuePoolBalance {
                id: "orchard".to_string(),
                chain_value: Zatoshis::new(2_000).expect("valid amount"),
                monitored: true,
                value_delta: Some(SignedZatoshis::try_new(-5).expect("valid delta")),
            }],
            upgrades: vec![NetworkUpgradeInfo {
                branch_id: ConsensusBranchId::new(0xc2d6_d0b4),
                name: "Canopy".to_string(),
                activation_height: Height::try_from(1_046_400).expect("valid height"),
                status: NetworkUpgradeStatus::Active,
            }],
            consensus: ConsensusBranchIds {
                chain_tip: ConsensusBranchId::new(0xc2d6_d0b4),
                next_block: ConsensusBranchId::new(0xc2d6_d0b4),
            },
        }
    }

    /// The full golden shape of `getblockchaininfo`: the exact sorted key set
    /// under zcashd's spellings, a typed value per key, the unnamed chain-supply
    /// total omitting `id` while a named pool carries it, the upgrade map keyed
    /// by branch id, and consensus branches as 8-digit hex. A dropped or
    /// mis-mapped field fails, because every scripted value is distinct and
    /// non-zero.
    #[test]
    fn blockchain_info_response_golden_shape() {
        let json =
            serde_json::to_value(blockchain_info_to_wire(scripted_info())).expect("serialize");
        assert_eq!(
            sorted_keys(&json),
            [
                "bestblockhash",
                "blocks",
                "chain",
                "chainSupply",
                "chainwork",
                "commitments",
                "consensus",
                "difficulty",
                "estimatedheight",
                "headers",
                "pruned",
                "size_on_disk",
                "upgrades",
                "valuePools",
                "verificationprogress",
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("chain").and_then(Value::as_str), Some("main"));
        assert_eq!(obj.get("blocks").and_then(Value::as_u64), Some(800_001));
        assert_eq!(obj.get("headers").and_then(Value::as_u64), Some(800_002));
        assert_eq!(
            obj.get("estimatedheight").and_then(Value::as_u64),
            Some(800_003)
        );
        assert_eq!(
            obj.get("bestblockhash").and_then(Value::as_str),
            Some("11".repeat(32).as_str())
        );
        assert_eq!(obj.get("difficulty").and_then(Value::as_f64), Some(123.5));
        assert_eq!(
            obj.get("verificationprogress").and_then(Value::as_f64),
            Some(0.75)
        );
        // 64-character big-endian hex, so the trailing `deadbeef` is zero-padded.
        assert_eq!(
            obj.get("chainwork").and_then(Value::as_str),
            Some(format!("{}deadbeef", "0".repeat(56)).as_str())
        );
        assert_eq!(obj.get("pruned").and_then(Value::as_bool), Some(true));
        assert_eq!(obj.get("size_on_disk").and_then(Value::as_u64), Some(4_096));
        assert_eq!(obj.get("commitments").and_then(Value::as_u64), Some(7));

        // chainSupply is the unnamed total: no `id`, exact zatoshis.
        let supply = obj.get("chainSupply").expect("chainSupply present");
        assert_eq!(sorted_keys(supply), ["chainValueZat", "monitored"]);
        let supply = supply.as_object().expect("an object");
        assert!(
            !supply.contains_key("id"),
            "the unnamed total omits id, it is not rendered as empty"
        );
        assert_eq!(
            supply.get("chainValueZat").and_then(Value::as_u64),
            Some(21_000_000)
        );
        assert_eq!(supply.get("monitored").and_then(Value::as_bool), Some(true));

        // A named pool carries its id and a signed delta.
        let pools = obj
            .get("valuePools")
            .and_then(Value::as_array)
            .expect("array");
        assert_eq!(pools.len(), 1);
        let pool = &pools[0];
        assert_eq!(
            sorted_keys(pool),
            ["chainValueZat", "id", "monitored", "valueDeltaZat"]
        );
        let pool = pool.as_object().expect("an object");
        assert_eq!(pool.get("id").and_then(Value::as_str), Some("orchard"));
        assert_eq!(
            pool.get("chainValueZat").and_then(Value::as_u64),
            Some(2_000)
        );
        assert_eq!(pool.get("valueDeltaZat").and_then(Value::as_i64), Some(-5));

        // The upgrade map is keyed by the branch id, written as 8-digit hex.
        let upgrades = obj.get("upgrades").expect("upgrades present");
        assert_eq!(sorted_keys(upgrades), ["c2d6d0b4"]);
        let upgrade = upgrades
            .as_object()
            .and_then(|m| m.get("c2d6d0b4"))
            .expect("the scripted upgrade");
        assert_eq!(sorted_keys(upgrade), ["activationheight", "name", "status"]);
        let upgrade = upgrade.as_object().expect("an object");
        assert_eq!(upgrade.get("name").and_then(Value::as_str), Some("Canopy"));
        assert_eq!(
            upgrade.get("activationheight").and_then(Value::as_u64),
            Some(1_046_400)
        );
        assert_eq!(
            upgrade.get("status").and_then(Value::as_str),
            Some("active")
        );

        // Consensus branches as 8-digit hex, both fields present.
        let consensus = obj.get("consensus").expect("consensus present");
        assert_eq!(sorted_keys(consensus), ["chaintip", "nextblock"]);
        let consensus = consensus.as_object().expect("an object");
        assert_eq!(
            consensus.get("chaintip").and_then(Value::as_str),
            Some("c2d6d0b4")
        );
        assert_eq!(
            consensus.get("nextblock").and_then(Value::as_str),
            Some("c2d6d0b4")
        );
    }

    /// Cumulative work the validator does not track is `None` in the domain and
    /// absent on the wire — never a zero a consumer could compare. Pins the
    /// `skip_serializing_if` on `chainwork`.
    #[test]
    fn blockchain_info_omits_chainwork_when_untracked() {
        let mut info = scripted_info();
        info.chain_work = None;
        let json = serde_json::to_value(blockchain_info_to_wire(info)).expect("serialize");
        assert!(
            !json
                .as_object()
                .expect("a JSON object")
                .contains_key("chainwork"),
            "untracked chainwork is omitted, not rendered as zero or null"
        );
    }

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

    /// The sorted key set of a JSON object, for pinning the exact wire shape:
    /// an extra key fails the comparison as surely as a missing one.
    fn sorted_keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("a JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// The two serde renames are the only thing aligning Rust field names with
    /// zcashd's: `address_type` must serialize as `address_type`, and
    /// `diversified_transmission_key` as `diversifiedtransmissionkey`. Pins the
    /// full key set in both the Sapling (all components present) and unified (all
    /// components absent) cases, so an added/dropped/renamed key fails; and pins
    /// that a `None` component is omitted rather than serialized as `null`.
    #[test]
    fn sapling_response_serializes_zcashd_field_names() {
        let json = serde_json::to_value(z_validated_to_wire(ZValidatedAddress::Sapling {
            address: "zs1sapling".to_string(),
            diversifier: [0u8; 11],
            diversified_transmission_key: [0u8; 32],
        }))
        .expect("serialize");
        assert_eq!(
            sorted_keys(&json),
            [
                "address",
                "address_type",
                "diversifiedtransmissionkey",
                "diversifier",
                "isvalid",
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("isvalid").and_then(|v| v.as_bool()), Some(true));
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
        assert_eq!(
            sorted_keys(&unified),
            ["address", "address_type", "isvalid"]
        );
        let unified = unified.as_object().expect("a JSON object");
        assert!(!unified.contains_key("diversifier"));
        assert!(!unified.contains_key("diversifiedtransmissionkey"));
    }
}
