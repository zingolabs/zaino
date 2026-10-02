//! Wire <-> domain conversion, owned by the adapter.
//!
//! Both directions live here: `*_from_hex` is the fallible external-input
//! validation (wire -> domain), and `to_hex` / the `*_to_wire` functions are the
//! domain -> wire renderings. No domain crate depends on any wire schema.

pub mod params;
pub mod response;

use zaino_address::{UnifiedReceivers, ValidatedAddress, ZValidatedAddress};
use zaino_primitives::types::rpc::BlockHeaderVerbose;
use zaino_primitives::types::AddressBalance;
use zaino_primitives::types::AddressDelta;
use zaino_primitives::types::BlockHash;
use zaino_primitives::types::TransactionId;
use zaino_primitives::types::{Block, BlockVerbose, Transaction};
use zaino_primitives::types::{
    BlockchainInfo, NetworkUpgradeInfo, NetworkUpgradeStatus, SignedZatoshis, ValuePoolBalance,
    Zatoshis,
};

use crate::error::RpcError;
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltaEntry, BlockHeaderResponse, BlockResponse,
    BlockchainInfoResponse, NetworkUpgradeResponse, TipConsensusResponse, TransactionInput,
    TransactionObject, TransactionOutput, UnifiedReceiversResponse, ValidateAddressResponse,
    ValuePoolResponse, ZValidateAddressResponse,
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

/// Decode a 32-byte block hash (wire -> domain input validation).
pub(crate) fn blockhash_from_hex(s: &str) -> Result<BlockHash, RpcError> {
    let arr: [u8; 32] = bytes_from_hex(s)?
        .try_into()
        .map_err(|_| RpcError::InvalidParams("block hash must be 32 bytes".into()))?;
    Ok(BlockHash::from(arr))
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

/// The number of zatoshis in one ZEC.
const ZATOSHIS_PER_ZEC: u64 = 100_000_000;

/// Formatting an exact zatoshi amount as its ZEC-denominated `f64` produced a
/// decimal that did not parse.
///
/// Carries its [`std::num::ParseFloatError`] cause rather than being asserted
/// away. The decimal is built from an integer whole part, an 8-digit fraction,
/// and at most one leading `-`, so no amount the domain can hold reaches this —
/// but the parse is typed as fallible, so the failure is surfaced with its cause
/// instead of panicking.
/// Public because it is carried by the public [`crate::RpcError`]; its
/// constructor and the functions that return it stay `pub(crate)`.
#[derive(Debug, thiserror::Error)]
#[error("zatoshi amount did not format to a parseable ZEC decimal")]
pub struct ZecFloatError(#[source] std::num::ParseFloatError);

/// Render an exact zatoshi magnitude (with its sign) as a ZEC-denominated `f64`
/// (domain -> wire).
///
/// zcashd reports amounts as a ZEC float beside the exact integer. The float is
/// derived by formatting the exact decimal `{whole}.{frac:08}` and parsing it —
/// correctly rounded, and with no `as` cast (there is no `From<u64>` for `f64`).
/// The split into magnitude and sign keeps a single leading `-`.
fn zatoshi_magnitude_to_zec(negative: bool, magnitude: u64) -> Result<f64, ZecFloatError> {
    let whole = magnitude / ZATOSHIS_PER_ZEC;
    let frac = magnitude % ZATOSHIS_PER_ZEC;
    let sign = if negative { "-" } else { "" };
    format!("{sign}{whole}.{frac:08}")
        .parse::<f64>()
        .map_err(ZecFloatError)
}

/// Render an unsigned zatoshi amount as a ZEC-denominated `f64` (domain -> wire).
///
/// The shared renderer for zcashd's `chainValue` / transaction `value` family.
pub(crate) fn zatoshis_to_zec(amount: Zatoshis) -> Result<f64, ZecFloatError> {
    zatoshi_magnitude_to_zec(false, amount.as_u64())
}

/// Render a signed zatoshi amount as a ZEC-denominated `f64` (domain -> wire),
/// preserving its sign.
///
/// The shared renderer for zcashd's `valueDelta` / `valueBalance` family.
/// [`i64::unsigned_abs`] takes the magnitude without an `as` cast and without
/// overflowing at [`i64::MIN`].
pub(crate) fn signed_zatoshis_to_zec(amount: SignedZatoshis) -> Result<f64, ZecFloatError> {
    let raw = amount.as_i64();
    zatoshi_magnitude_to_zec(raw.is_negative(), raw.unsigned_abs())
}

/// Render one value pool for the wire (domain -> wire). Each amount is emitted
/// twice: as zcashd's ZEC float (the key clients read) and as the exact zatoshi
/// integer beside it. An empty id (the unnamed chain-supply total) is omitted by
/// the response type.
fn value_pool_to_wire(pool: &ValuePoolBalance) -> Result<ValuePoolResponse, ZecFloatError> {
    Ok(ValuePoolResponse {
        id: pool.id.clone(),
        monitored: pool.monitored,
        chain_value: zatoshis_to_zec(pool.chain_value)?,
        chain_value_zat: pool.chain_value.as_u64(),
        value_delta: pool.value_delta.map(signed_zatoshis_to_zec).transpose()?,
        value_delta_zat: pool.value_delta.map(|delta| delta.as_i64()),
    })
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
pub(crate) fn blockchain_info_to_wire(
    info: BlockchainInfo,
) -> Result<BlockchainInfoResponse, ZecFloatError> {
    Ok(BlockchainInfoResponse {
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
        chain_supply: value_pool_to_wire(&info.chain_supply)?,
        value_pools: info
            .value_pools
            .iter()
            .map(value_pool_to_wire)
            .collect::<Result<_, _>>()?,
        upgrades: info.upgrades.iter().map(upgrade_to_wire).collect(),
        consensus: TipConsensusResponse {
            chaintip: info.consensus.chain_tip.to_string(),
            nextblock: info.consensus.next_block.to_string(),
        },
    })
}

/// Render a verbose block header as the `getblockheader` response
/// (domain -> wire).
///
/// `bits` is the 8-digit hex nBits; the nonce and solution are hex; `chainwork`
/// is 64-character big-endian hex, omitted when the validator does not track it.
/// `block_commitments` is not emitted: the explorer's blocks-by-date list, the
/// sole consumer of this method, does not render it.
pub(crate) fn block_header_to_wire(header: BlockHeaderVerbose) -> BlockHeaderResponse {
    BlockHeaderResponse {
        hash: to_hex(header.hash.into()),
        confirmations: header.confirmations,
        height: header.height.into(),
        version: header.version,
        merkle_root: to_hex(header.merkle_root.into()),
        final_sapling_root: header.final_sapling_root.map(|root| to_hex(root.into())),
        time: header.time,
        nonce: to_hex(header.nonce),
        solution: bytes_to_hex(&header.solution),
        bits: format!("{:08x}", header.bits.as_bits()),
        difficulty: header.difficulty,
        chainwork: header
            .chainwork
            .map(|work| bytes_to_hex(&work.to_be_bytes())),
        previous_block_hash: header.previous_block_hash.map(|hash| to_hex(hash.into())),
        next_block_hash: header.next_block_hash.map(|hash| to_hex(hash.into())),
    }
}

/// Render one transaction for a verbose response (domain -> wire). The single
/// transaction renderer: `getblock` verbosity 2 calls it per transaction, and
/// Task 5c extends it for `getrawtransaction` verbosity 1.
///
/// `is_coinbase` is the caller's knowledge of position: the coinbase is the
/// block's first transaction, and the domain drops its coinbase input, so the
/// coinbase marker cannot be recovered from the transaction's own fields and is
/// supplied by the caller. A coinbase renders a single `{coinbase: true}` input.
///
/// A spend input carries `txid` and `vout` only — **not** the spent output's
/// `address` or `value`. Those describe the *spent output*, which lives in an
/// earlier transaction rather than in this one's bytes; resolving them needs a
/// prevout lookup deferred to a follow-up (ruling R37).
pub(crate) fn transaction_to_wire(
    transaction: &Transaction,
    is_coinbase: bool,
) -> TransactionObject {
    let vin = if is_coinbase {
        vec![TransactionInput::Coinbase { coinbase: true }]
    } else {
        transaction
            .transparent
            .inputs
            .iter()
            .map(|input| TransactionInput::Spend {
                txid: to_hex(input.prev_txid.into()),
                vout: input.prev_index,
            })
            .collect()
    };
    let vout = transaction
        .transparent
        .outputs
        .iter()
        .enumerate()
        .map(|(n, output)| TransactionOutput {
            value_zat: output.value.as_u64(),
            // The output index is a `u32` in the domain (`OutputIndex`); the
            // consensus block-size limit bounds output counts far below 2^32, so
            // the conversion restores that type and fails loud if ever violated.
            n: u32::try_from(n).expect("a transaction has fewer than 2^32 outputs"),
        })
        .collect();
    TransactionObject {
        txid: to_hex(transaction.txid.into()),
        vin,
        vout,
        vjoinsplit: Vec::new(),
    }
}

/// Render a block and its chain-position facts as the `getblock` verbosity-2
/// response (domain -> wire). The header fields come from the block, the
/// confirmations/difficulty/chainwork from [`BlockVerbose`], and each
/// transaction through [`transaction_to_wire`] (the first is the coinbase).
pub(crate) fn block_to_wire(block: Block, verbose: BlockVerbose) -> BlockResponse {
    let header = &block.header;
    let tx = block
        .transactions
        .iter()
        .enumerate()
        .map(|(index, transaction)| transaction_to_wire(transaction, index == 0))
        .collect();
    BlockResponse {
        hash: to_hex(header.hash.into()),
        confirmations: verbose.confirmations,
        height: header.height.into(),
        version: header.version,
        merkle_root: to_hex(header.merkle_root.into()),
        time: header.time,
        nonce: to_hex(header.nonce),
        bits: format!("{:08x}", header.bits.as_bits()),
        difficulty: verbose.difficulty,
        chainwork: verbose
            .chainwork
            .map(|work| bytes_to_hex(&work.to_be_bytes())),
        tx,
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
            kind: None,
            diversifier: None,
            diversified_transmission_key: None,
        },
        ZValidatedAddress::P2pkh { address } => ZValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            address_type: Some("p2pkh".to_string()),
            kind: Some("p2pkh".to_string()),
            diversifier: None,
            diversified_transmission_key: None,
        },
        ZValidatedAddress::P2sh { address } => ZValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            address_type: Some("p2sh".to_string()),
            kind: Some("p2sh".to_string()),
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
            kind: Some("sapling".to_string()),
            diversifier: Some(bytes_to_hex(&diversifier)),
            diversified_transmission_key: Some(bytes_to_hex(&diversified_transmission_key)),
        },
        ZValidatedAddress::Unified { address } => ZValidateAddressResponse {
            isvalid: true,
            address: Some(address),
            address_type: Some("unified".to_string()),
            kind: Some("unified".to_string()),
            diversifier: None,
            diversified_transmission_key: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        block_header_to_wire, block_to_wire, blockchain_info_to_wire, signed_zatoshis_to_zec,
        transaction_to_wire, validated_to_wire, z_validated_to_wire, zatoshis_to_zec,
    };
    use serde_json::Value;
    use zaino_address::{ValidatedAddress, ZValidatedAddress};
    use zaino_primitives::types::rpc::BlockHeaderVerbose;
    use zaino_primitives::types::{
        AbsoluteChainWork, Block, BlockHash, BlockHeader, BlockTreeSizes, BlockVerbose,
        BlockchainInfo, ChainMetadata, CompactDifficulty, ConsensusBranchId, ConsensusBranchIds,
        EquihashSolution, Height, NetworkUpgradeInfo, NetworkUpgradeStatus, Script, SignedZatoshis,
        Transaction, TransactionId, TransparentData, TransparentInput, TransparentOutput, TreeSize,
        ValuePoolBalance, Zatoshis,
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
        let json = serde_json::to_value(
            blockchain_info_to_wire(scripted_info()).expect("scripted info renders"),
        )
        .expect("serialize");
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

        // chainSupply is the unnamed total: no `id`, the ZEC float beside the
        // exact zatoshis. 21_000_000 zat = 0.21 ZEC.
        let supply = obj.get("chainSupply").expect("chainSupply present");
        assert_eq!(
            sorted_keys(supply),
            ["chainValue", "chainValueZat", "monitored"]
        );
        let supply = supply.as_object().expect("an object");
        assert!(
            !supply.contains_key("id"),
            "the unnamed total omits id, it is not rendered as empty"
        );
        assert_eq!(supply.get("chainValue").and_then(Value::as_f64), Some(0.21));
        assert_eq!(
            supply.get("chainValueZat").and_then(Value::as_u64),
            Some(21_000_000)
        );
        assert_eq!(supply.get("monitored").and_then(Value::as_bool), Some(true));

        // A named pool carries its id and a signed delta, each amount as both the
        // ZEC float the client reads and the exact zatoshis. 2_000 zat =
        // 0.00002 ZEC; a -5 zat delta = -0.00000005 ZEC.
        let pools = obj
            .get("valuePools")
            .and_then(Value::as_array)
            .expect("array");
        assert_eq!(pools.len(), 1);
        let pool = &pools[0];
        assert_eq!(
            sorted_keys(pool),
            [
                "chainValue",
                "chainValueZat",
                "id",
                "monitored",
                "valueDelta",
                "valueDeltaZat",
            ]
        );
        let pool = pool.as_object().expect("an object");
        assert_eq!(pool.get("id").and_then(Value::as_str), Some("orchard"));
        assert_eq!(
            pool.get("chainValue").and_then(Value::as_f64),
            Some(0.00002)
        );
        assert_eq!(
            pool.get("chainValueZat").and_then(Value::as_u64),
            Some(2_000)
        );
        assert_eq!(
            pool.get("valueDelta").and_then(Value::as_f64),
            Some(-0.00000005)
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
        let json = serde_json::to_value(blockchain_info_to_wire(info).expect("renders"))
            .expect("serialize");
        assert!(
            !json
                .as_object()
                .expect("a JSON object")
                .contains_key("chainwork"),
            "untracked chainwork is omitted, not rendered as zero or null"
        );
    }

    /// A verbose block header with a distinguishable, non-zero value in every
    /// field, so a golden assertion fails if any field is dropped, defaulted or
    /// mis-mapped. `block_commitments` is set so the test proves it is *not*
    /// emitted.
    fn scripted_header() -> BlockHeaderVerbose {
        let mut work_bytes = [0u8; 32];
        work_bytes[28..].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        BlockHeaderVerbose {
            hash: BlockHash::from([0x11; 32]),
            confirmations: 7,
            height: Height::try_from(2_468).expect("valid height"),
            version: 4,
            merkle_root: [0x22; 32].into(),
            final_sapling_root: Some([0x33; 32].into()),
            time: 1_600_000_000,
            nonce: [0x44; 32],
            solution: vec![0xaa, 0xbb, 0xcc],
            bits: CompactDifficulty::try_from_bits(0x1f07_ffff).expect("valid nBits"),
            difficulty: 123.5,
            block_commitments: Some([0x55; 32].into()),
            chainwork: AbsoluteChainWork::try_from_reported(work_bytes).expect("in-range work"),
            previous_block_hash: Some(BlockHash::from([0x66; 32])),
            next_block_hash: Some(BlockHash::from([0x77; 32])),
        }
    }

    /// The full golden shape of `getblockheader`: the exact sorted key set under
    /// zcashd's spellings, a typed value per key, `bits` as 8-digit hex, the
    /// nonce and solution as hex, and `chainwork` as 64-character big-endian hex.
    /// `blockcommitments` is absent: the explorer's blocks-by-date list does not
    /// render it, so the conversion does not emit it even though the domain
    /// carries it (a deliberate omission, not a bug).
    #[test]
    fn block_header_response_golden_shape() {
        let json =
            serde_json::to_value(block_header_to_wire(scripted_header())).expect("serialize");
        assert_eq!(
            sorted_keys(&json),
            [
                "bits",
                "chainwork",
                "confirmations",
                "difficulty",
                "finalsaplingroot",
                "hash",
                "height",
                "merkleroot",
                "nextblockhash",
                "nonce",
                "previousblockhash",
                "solution",
                "time",
                "version",
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("hash").and_then(Value::as_str),
            Some("11".repeat(32).as_str())
        );
        assert_eq!(obj.get("confirmations").and_then(Value::as_i64), Some(7));
        assert_eq!(obj.get("height").and_then(Value::as_u64), Some(2_468));
        assert_eq!(obj.get("version").and_then(Value::as_u64), Some(4));
        assert_eq!(
            obj.get("merkleroot").and_then(Value::as_str),
            Some("22".repeat(32).as_str())
        );
        assert_eq!(
            obj.get("finalsaplingroot").and_then(Value::as_str),
            Some("33".repeat(32).as_str())
        );
        assert_eq!(obj.get("time").and_then(Value::as_u64), Some(1_600_000_000));
        assert_eq!(
            obj.get("nonce").and_then(Value::as_str),
            Some("44".repeat(32).as_str())
        );
        assert_eq!(obj.get("solution").and_then(Value::as_str), Some("aabbcc"));
        // nBits as 8-digit hex, not an integer.
        assert_eq!(obj.get("bits").and_then(Value::as_str), Some("1f07ffff"));
        assert_eq!(obj.get("difficulty").and_then(Value::as_f64), Some(123.5));
        // 64-character big-endian hex, trailing deadbeef zero-padded.
        assert_eq!(
            obj.get("chainwork").and_then(Value::as_str),
            Some(format!("{}deadbeef", "0".repeat(56)).as_str())
        );
        assert_eq!(
            obj.get("previousblockhash").and_then(Value::as_str),
            Some("66".repeat(32).as_str())
        );
        assert_eq!(
            obj.get("nextblockhash").and_then(Value::as_str),
            Some("77".repeat(32).as_str())
        );
        // The domain carries block_commitments, but this method does not emit it.
        assert!(
            !obj.contains_key("blockcommitments"),
            "block commitments are deliberately not rendered by getblockheader"
        );
    }

    /// Genesis/tip/pre-Sapling/Zebra case: the four optional fields are absent,
    /// not `null`. Pins each `skip_serializing_if`.
    #[test]
    fn block_header_response_omits_absent_optionals() {
        let header = BlockHeaderVerbose {
            final_sapling_root: None,
            chainwork: None,
            previous_block_hash: None,
            next_block_hash: None,
            ..scripted_header()
        };
        let json = serde_json::to_value(block_header_to_wire(header)).expect("serialize");
        assert_eq!(
            sorted_keys(&json),
            [
                "bits",
                "confirmations",
                "difficulty",
                "hash",
                "height",
                "merkleroot",
                "nonce",
                "solution",
                "time",
                "version",
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        for absent in [
            "finalsaplingroot",
            "chainwork",
            "previousblockhash",
            "nextblockhash",
        ] {
            assert!(
                !obj.contains_key(absent),
                "an absent {absent} is omitted, not rendered as null"
            );
        }
    }

    fn empty_pools() -> (
        zaino_primitives::types::SaplingData,
        zaino_primitives::types::OrchardData,
        zaino_primitives::types::OrchardData,
    ) {
        Default::default()
    }

    /// A non-coinbase transaction renders its spends as `{txid, vout}` (no
    /// `address` or `value` — ruling R37) and its outputs as `{valueZat, n}` with
    /// an ascending index, and an always-empty `vjoinsplit`.
    #[test]
    fn transaction_to_wire_renders_spends_and_outputs() {
        let (sapling, orchard, ironwood) = empty_pools();
        let tx = Transaction {
            txid: TransactionId::from([0xAB; 32]),
            transparent: TransparentData {
                inputs: vec![TransparentInput {
                    prev_txid: TransactionId::from([0x01; 32]),
                    prev_index: 3,
                }],
                outputs: vec![
                    TransparentOutput {
                        value: Zatoshis::new(1_000).expect("valid amount"),
                        script: Script::new(vec![]),
                    },
                    TransparentOutput {
                        value: Zatoshis::new(2_000).expect("valid amount"),
                        script: Script::new(vec![]),
                    },
                ],
            },
            sapling,
            orchard,
            ironwood,
        };
        let json = serde_json::to_value(transaction_to_wire(&tx, false)).expect("serialize");
        assert_eq!(sorted_keys(&json), ["txid", "vin", "vjoinsplit", "vout"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("txid").and_then(Value::as_str),
            Some("ab".repeat(32).as_str())
        );

        let vin = obj.get("vin").and_then(Value::as_array).expect("vin array");
        assert_eq!(vin.len(), 1);
        assert_eq!(sorted_keys(&vin[0]), ["txid", "vout"]);
        let input = vin[0].as_object().expect("an object");
        assert_eq!(
            input.get("txid").and_then(Value::as_str),
            Some("01".repeat(32).as_str())
        );
        assert_eq!(input.get("vout").and_then(Value::as_u64), Some(3));
        assert!(
            !input.contains_key("address") && !input.contains_key("value"),
            "a spend input carries no address or value (ruling R37)"
        );

        let vout = obj
            .get("vout")
            .and_then(Value::as_array)
            .expect("vout array");
        assert_eq!(vout.len(), 2);
        assert_eq!(sorted_keys(&vout[0]), ["n", "valueZat"]);
        let first = vout[0].as_object().expect("an object");
        assert_eq!(first.get("valueZat").and_then(Value::as_u64), Some(1_000));
        assert_eq!(first.get("n").and_then(Value::as_u64), Some(0));
        let second = vout[1].as_object().expect("an object");
        assert_eq!(second.get("valueZat").and_then(Value::as_u64), Some(2_000));
        assert_eq!(second.get("n").and_then(Value::as_u64), Some(1));

        assert_eq!(
            obj.get("vjoinsplit")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0),
            "vjoinsplit is always an empty array"
        );
    }

    /// The coinbase transaction renders a single `{coinbase: true}` input, even
    /// though the domain drops the coinbase input, so the marker comes from the
    /// caller's `is_coinbase`, not the transaction's own fields.
    #[test]
    fn transaction_to_wire_renders_the_coinbase_marker() {
        let (sapling, orchard, ironwood) = empty_pools();
        let tx = Transaction {
            txid: TransactionId::from([0xCD; 32]),
            transparent: TransparentData::default(),
            sapling,
            orchard,
            ironwood,
        };
        let json = serde_json::to_value(transaction_to_wire(&tx, true)).expect("serialize");
        let obj = json.as_object().expect("a JSON object");
        let vin = obj.get("vin").and_then(Value::as_array).expect("vin array");
        assert_eq!(vin.len(), 1);
        assert_eq!(sorted_keys(&vin[0]), ["coinbase"]);
        let input = vin[0].as_object().expect("an object");
        assert_eq!(input.get("coinbase").and_then(Value::as_bool), Some(true));
        assert!(
            !input.contains_key("txid"),
            "the coinbase input is a marker, not a spend"
        );
    }

    /// A block header with distinguishable values, for the block golden test.
    fn scripted_block_header() -> BlockHeader {
        BlockHeader {
            hash: BlockHash::from([0x11; 32]),
            version: 4,
            prev_hash: BlockHash::from([0x22; 32]),
            height: Height::try_from(2_468).expect("valid height"),
            time: 1_600_000_000,
            merkle_root: [0x33; 32].into(),
            block_commitments: [0x44; 32].into(),
            bits: CompactDifficulty::try_from_bits(0x1f07_ffff).expect("valid nBits"),
            nonce: [0x55; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        }
    }

    /// Chain-position facts for the block golden test.
    fn scripted_block_verbose() -> BlockVerbose {
        let mut work_bytes = [0u8; 32];
        work_bytes[28..].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        BlockVerbose {
            confirmations: 9,
            difficulty: 123.5,
            chainwork: AbsoluteChainWork::try_from_reported(work_bytes).expect("in-range work"),
            chain_supply: None,
            value_pools: Vec::new(),
            tree_sizes: BlockTreeSizes {
                sapling: TreeSize::from(1u32),
                orchard: TreeSize::from(2u32),
                ironwood: TreeSize::from(3u32),
            },
            next_block_hash: Some(BlockHash::from([0x66; 32])),
        }
    }

    fn coinbase_and_spend_block() -> Block {
        let (sapling, orchard, ironwood) = empty_pools();
        let coinbase = Transaction {
            txid: TransactionId::from([0xC0; 32]),
            transparent: TransparentData {
                inputs: Vec::new(),
                outputs: vec![TransparentOutput {
                    value: Zatoshis::new(625_000_000).expect("valid amount"),
                    script: Script::new(vec![]),
                }],
            },
            sapling: sapling.clone(),
            orchard: orchard.clone(),
            ironwood: ironwood.clone(),
        };
        let spend = Transaction {
            txid: TransactionId::from([0x7A; 32]),
            transparent: TransparentData {
                inputs: vec![TransparentInput {
                    prev_txid: TransactionId::from([0x01; 32]),
                    prev_index: 0,
                }],
                outputs: vec![TransparentOutput {
                    value: Zatoshis::new(500).expect("valid amount"),
                    script: Script::new(vec![]),
                }],
            },
            sapling,
            orchard,
            ironwood,
        };
        Block {
            header: scripted_block_header(),
            transactions: vec![coinbase, spend],
            chain_metadata: ChainMetadata::ZERO,
        }
    }

    /// The full golden shape of `getblock` at verbosity 2: the header fields, the
    /// chain-position facts, and the decoded transactions. `size` is absent — a
    /// known divergence from zcashd: re-serializing the block to measure it needs
    /// the validator's chain library this adapter lacks, so it is deferred to the
    /// same follow-up as input prevout resolution. The first transaction renders
    /// as the coinbase, the second as a spend.
    #[test]
    fn block_response_golden_shape() {
        let json = serde_json::to_value(block_to_wire(
            coinbase_and_spend_block(),
            scripted_block_verbose(),
        ))
        .expect("serialize");
        assert_eq!(
            sorted_keys(&json),
            [
                "bits",
                "chainwork",
                "confirmations",
                "difficulty",
                "hash",
                "height",
                "merkleroot",
                "nonce",
                "time",
                "tx",
                "version",
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert!(
            !obj.contains_key("size"),
            "block size is a known divergence: omitted, not rendered"
        );
        // Header fields come from the block.
        assert_eq!(
            obj.get("hash").and_then(Value::as_str),
            Some("11".repeat(32).as_str())
        );
        assert_eq!(obj.get("height").and_then(Value::as_u64), Some(2_468));
        assert_eq!(obj.get("version").and_then(Value::as_u64), Some(4));
        assert_eq!(
            obj.get("merkleroot").and_then(Value::as_str),
            Some("33".repeat(32).as_str())
        );
        assert_eq!(obj.get("time").and_then(Value::as_u64), Some(1_600_000_000));
        assert_eq!(
            obj.get("nonce").and_then(Value::as_str),
            Some("55".repeat(32).as_str())
        );
        assert_eq!(obj.get("bits").and_then(Value::as_str), Some("1f07ffff"));
        // Chain-position facts come from BlockVerbose.
        assert_eq!(obj.get("confirmations").and_then(Value::as_i64), Some(9));
        assert_eq!(obj.get("difficulty").and_then(Value::as_f64), Some(123.5));
        assert_eq!(
            obj.get("chainwork").and_then(Value::as_str),
            Some(format!("{}deadbeef", "0".repeat(56)).as_str())
        );
        // The transactions: coinbase first, spend second.
        let tx = obj.get("tx").and_then(Value::as_array).expect("tx array");
        assert_eq!(tx.len(), 2);
        let coinbase_vin = tx[0]
            .as_object()
            .and_then(|t| t.get("vin"))
            .and_then(Value::as_array)
            .expect("coinbase vin");
        assert_eq!(sorted_keys(&coinbase_vin[0]), ["coinbase"]);
        let spend_vin = tx[1]
            .as_object()
            .and_then(|t| t.get("vin"))
            .and_then(Value::as_array)
            .expect("spend vin");
        assert_eq!(sorted_keys(&spend_vin[0]), ["txid", "vout"]);
    }

    /// Chainwork the validator does not track is absent on the wire, not zero or
    /// null. Pins the `skip_serializing_if` on the block's `chainwork`.
    #[test]
    fn block_response_omits_chainwork_when_untracked() {
        let mut verbose = scripted_block_verbose();
        verbose.chainwork = None;
        let json = serde_json::to_value(block_to_wire(coinbase_and_spend_block(), verbose))
            .expect("serialize");
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
                "type",
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("isvalid").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            obj.get("address_type").and_then(|v| v.as_str()),
            Some("sapling")
        );
        // The legacy `type` key carries the identical kind; the explorer's search
        // pattern-matches on it, so its absence crashes the LiveView.
        assert_eq!(obj.get("type").and_then(|v| v.as_str()), Some("sapling"));
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
            ["address", "address_type", "isvalid", "type"]
        );
        let unified = unified.as_object().expect("a JSON object");
        assert_eq!(
            unified.get("type").and_then(|v| v.as_str()),
            Some("unified")
        );
        assert!(!unified.contains_key("diversifier"));
        assert!(!unified.contains_key("diversifiedtransmissionkey"));
    }

    /// The ZEC-float renderer is correctly rounded and signed, across the amounts
    /// the follow-up tasks reuse it for. The parse of a self-formatted decimal is
    /// infallible in practice; the test uses the fallible API honestly.
    #[test]
    fn zatoshis_render_as_correctly_rounded_zec() {
        // Zero, the smallest unit, one ZEC, and the supply ceiling.
        assert_eq!(zatoshis_to_zec(Zatoshis::ZERO).expect("renders"), 0.0);
        assert_eq!(
            zatoshis_to_zec(Zatoshis::new(1).expect("valid")).expect("renders"),
            0.00000001
        );
        assert_eq!(
            zatoshis_to_zec(Zatoshis::new(100_000_000).expect("valid")).expect("renders"),
            1.0
        );
        // MAX_MONEY (Zatoshis::MAX) is 21_000_000 ZEC exactly.
        assert_eq!(
            zatoshis_to_zec(Zatoshis::MAX).expect("renders"),
            21_000_000.0
        );

        // A negative delta keeps a single leading sign. -150_000_000 zat = -1.5 ZEC.
        assert_eq!(
            signed_zatoshis_to_zec(SignedZatoshis::try_new(-150_000_000).expect("valid"))
                .expect("renders"),
            -1.5
        );

        // Rounding is visible: 0.3 ZEC has no exact f64, so this pins that the
        // decimal is correctly rounded to the nearest f64 (the `0.3_f64` literal,
        // which prints as 0.3 but is actually 0.299999999999999988…).
        assert_eq!(
            zatoshis_to_zec(Zatoshis::new(30_000_000).expect("valid")).expect("renders"),
            0.3_f64
        );
    }
}
