//! Wire <-> domain conversion, owned by the adapter.
//!
//! Both directions live here: `*_from_hex` is the fallible external-input
//! validation (wire -> domain), and `to_hex` / the `*_to_wire` functions are the
//! domain -> wire renderings. No domain crate depends on any wire schema.

pub mod params;
pub mod response;

use zaino_address::{
    transparent_address_from_script, UnifiedReceivers, ValidatedAddress, ZValidatedAddress,
};
use zaino_primitives::types::rpc::BlockHeaderVerbose;
use zaino_primitives::types::AddressBalance;
use zaino_primitives::types::AddressDelta;
use zaino_primitives::types::BlockHash;
use zaino_primitives::types::TransactionId;
use zaino_primitives::types::{Block, BlockVerbose, Script, Transaction};
use zaino_primitives::types::{
    BlockchainInfo, NetworkUpgradeInfo, NetworkUpgradeStatus, SignedZatoshis, ValuePoolBalance,
    Zatoshis,
};
use zaino_service::{BlockTransactionViews, TransactionView};
use zcash_protocol::consensus::Network;

use crate::error::RpcError;
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltaEntry, BlockHeaderResponse, BlockResponse,
    BlockchainInfoResponse, JoinSplitObject, NetworkUpgradeResponse, OrchardActionObject,
    OrchardObject, ScriptPubKey, ShieldedOutput, ShieldedSpend, TipConsensusResponse,
    TransactionInput, TransactionObject, TransactionOutput, UnifiedReceiversResponse,
    ValidateAddressResponse, ValuePoolResponse, ZValidateAddressResponse,
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

/// The number of zatoshis in one ZEC, as the exact ZEC-float divisor.
const ZATOSHIS_PER_ZEC: f64 = 100_000_000.0;

/// `2^32`: the weight of the high half when recombining a `u64` from its two
/// 32-bit halves.
const TWO_POW_32: f64 = 4_294_967_296.0;

/// Render an exact zatoshi magnitude (with its sign) as a ZEC-denominated `f64`
/// (domain -> wire).
///
/// Exact and `as`-free by construction. [`Zatoshis`] and [`SignedZatoshis`] bound
/// every amount, and so every magnitude, to at most MAX_MONEY
/// (21_000_000 × 100_000_000 = 2_100_000_000_000_000), which is below `2^51`.
/// That bound lives in the input type, so no runtime check is needed:
///
/// - The magnitude is split into its low and high 32-bit halves. Each half is
///   below `2^32`, so [`f64::from`] widens it losslessly (there is no
///   `From<u64>` for `f64`, which is why the split is necessary).
/// - Recombining as `high × 2^32 + low` is exact, because the magnitude is below
///   `2^53`, the largest integer every `f64` represents exactly.
/// - The single IEEE division by [`ZATOSHIS_PER_ZEC`] is correctly rounded.
///
/// The result is therefore bit-identical to parsing the exact decimal
/// `{whole}.{frac:08}`. The sign is applied last; a zero magnitude is never
/// negative, so it renders as `0.0`, not `-0.0`.
fn zatoshi_magnitude_to_zec(negative: bool, magnitude: u64) -> f64 {
    let bytes = magnitude.to_le_bytes();
    let low = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let high = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let zec = (f64::from(high) * TWO_POW_32 + f64::from(low)) / ZATOSHIS_PER_ZEC;
    if negative {
        -zec
    } else {
        zec
    }
}

/// Render an unsigned zatoshi amount as a ZEC-denominated `f64` (domain -> wire).
///
/// The shared renderer for zcashd's `chainValue` / transaction `value` family.
fn zatoshis_to_zec(amount: Zatoshis) -> f64 {
    zatoshi_magnitude_to_zec(false, amount.as_u64())
}

/// Render a signed zatoshi amount as a ZEC-denominated `f64` (domain -> wire),
/// preserving its sign.
///
/// The shared renderer for zcashd's `valueDelta` / `valueBalance` family.
/// [`i64::unsigned_abs`] takes the magnitude without an `as` cast and without
/// overflowing at [`i64::MIN`].
fn signed_zatoshis_to_zec(amount: SignedZatoshis) -> f64 {
    let raw = amount.as_i64();
    zatoshi_magnitude_to_zec(raw.is_negative(), raw.unsigned_abs())
}

/// Render one value pool for the wire (domain -> wire). Each amount is emitted
/// twice: as zcashd's ZEC float (the key clients read) and as the exact zatoshi
/// integer beside it. An empty id (the unnamed chain-supply total) is omitted by
/// the response type.
fn value_pool_to_wire(pool: &ValuePoolBalance) -> ValuePoolResponse {
    ValuePoolResponse {
        id: pool.id.clone(),
        monitored: pool.monitored,
        chain_value: zatoshis_to_zec(pool.chain_value),
        chain_value_zat: pool.chain_value.as_u64(),
        value_delta: pool.value_delta.map(signed_zatoshis_to_zec),
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

/// The P2PKH template's leading opcodes (`OP_DUP OP_HASH160 <push 20>`), used
/// only to tell a decoded P2PKH output from a P2SH one for the `type` tag. The
/// canonical decode lives in [`zaino_address::transparent_address_from_script`];
/// this reads the kind off a script that already decoded.
const P2PKH_SCRIPT_PREFIX: [u8; 3] = [0x76, 0xa9, 0x14];

/// Render a transparent output's locking script (domain -> wire). `addresses`
/// (a one-element array) and `type` are present together when the script is a
/// standard P2PKH/P2SH template, and absent together otherwise — a non-standard
/// script is not an address, so the explorer gets no `addresses` key to iterate.
fn script_pub_key_to_wire(script: &Script, network: &Network) -> ScriptPubKey {
    let bytes: Vec<u8> = script.clone().into();
    let (addresses, script_type) = match transparent_address_from_script(&bytes, network) {
        Some(address) => {
            let kind = if bytes.starts_with(&P2PKH_SCRIPT_PREFIX) {
                "pubkeyhash"
            } else {
                "scripthash"
            };
            (Some(vec![address]), Some(kind.to_string()))
        }
        None => (None, None),
    };
    ScriptPubKey {
        hex: bytes_to_hex(&bytes),
        addresses,
        script_type,
    }
}

/// Render one transaction's inputs (domain -> wire). A coinbase renders its one
/// coinbase input from [`TransactionDetail::coinbase`](zaino_primitives::types::TransactionDetail::coinbase)
/// — coinbase-ness is data, not block position. Every other transaction renders
/// its resolved spends, each carrying the value and address of the output it
/// spends.
fn inputs_to_wire(view: &TransactionView, network: &Network) -> Vec<TransactionInput> {
    if let Some(coinbase) = &view.detail.coinbase {
        let script: Vec<u8> = coinbase.script.clone().into();
        return vec![TransactionInput::Coinbase {
            coinbase: bytes_to_hex(&script),
            sequence: coinbase.sequence,
        }];
    }
    view.inputs
        .iter()
        .map(|input| TransactionInput::Spend {
            txid: to_hex(input.outpoint.prev_txid.into()),
            vout: input.outpoint.prev_index,
            value: zatoshis_to_zec(input.spent.value),
            value_sat: input.spent.value.as_u64(),
            address: {
                let script: Vec<u8> = input.spent.script.clone().into();
                transparent_address_from_script(&script, network)
            },
        })
        .collect()
}

/// Render one transaction's outputs (domain -> wire). `n` comes from a `u32`
/// range zipped with the outputs, never a fallible index cast.
fn outputs_to_wire(transaction: &Transaction, network: &Network) -> Vec<TransactionOutput> {
    transaction
        .transparent
        .outputs
        .iter()
        .zip(0u32..)
        .map(|(output, n)| TransactionOutput {
            value: zatoshis_to_zec(output.value),
            value_zat: output.value.as_u64(),
            n,
            script_pub_key: script_pub_key_to_wire(&output.script, network),
        })
        .collect()
}

/// Render one resolved transaction in the explorer's shape (domain -> wire). The
/// single transaction renderer, shared by `getrawtransaction` verbosity 1 and
/// `getblock` verbosity 2.
///
/// The conditional keys follow the envelope: `versiongroupid`/`expiryheight` on
/// an overwintered transaction; the Sapling `valueBalance` family and the
/// `vShielded*` arrays from version 4; the `orchard` bundle from version 5. The
/// Ironwood pool is deliberately not rendered — the explorer has no field for it.
pub(crate) fn transaction_view_to_wire(
    view: &TransactionView,
    network: &Network,
) -> TransactionObject {
    let transaction = &view.transaction;
    let detail = &view.detail;

    let vjoinsplit = detail
        .joinsplits
        .iter()
        .map(|js| JoinSplitObject {
            vpub_old: zatoshis_to_zec(js.vpub_old),
            vpub_old_zat: js.vpub_old.as_u64(),
            vpub_new: zatoshis_to_zec(js.vpub_new),
            vpub_new_zat: js.vpub_new.as_u64(),
        })
        .collect();

    // Sapling fields appear from version 4.
    let sapling = &transaction.sapling;
    let value_balance =
        (detail.version >= 4).then(|| signed_zatoshis_to_zec(sapling.value_balance));
    let value_balance_zat = (detail.version >= 4).then(|| sapling.value_balance.as_i64());
    let shielded_spends = (detail.version >= 4).then(|| {
        sapling
            .spends
            .iter()
            .map(|spend| ShieldedSpend {
                nullifier: to_hex(spend.nullifier.into()),
            })
            .collect()
    });
    let shielded_outputs = (detail.version >= 4).then(|| {
        sapling
            .outputs
            .iter()
            .map(|output| ShieldedOutput {
                cmu: to_hex(output.cmu.into()),
                ephemeral_key: to_hex(output.ephemeral_key.into()),
            })
            .collect()
    });

    // The Orchard bundle appears from version 5, even with no actions.
    let orchard = (detail.version >= 5).then(|| OrchardObject {
        actions: transaction
            .orchard
            .actions
            .iter()
            .map(|action| OrchardActionObject {
                nullifier: to_hex(action.nullifier.into()),
                cmx: to_hex(action.cmx.into()),
                ephemeral_key: to_hex(action.ephemeral_key.into()),
            })
            .collect(),
        value_balance: signed_zatoshis_to_zec(transaction.orchard.value_balance),
        value_balance_zat: transaction.orchard.value_balance.as_i64(),
    });

    TransactionObject {
        txid: to_hex(transaction.txid.into()),
        version: detail.version,
        overwintered: detail.overwintered,
        version_group_id: detail.version_group_id.map(|id| format!("{id:08x}")),
        locktime: detail.lock_time,
        expiry_height: detail.expiry_height.map(Into::into),
        size: detail.size,
        vin: inputs_to_wire(view, network),
        vout: outputs_to_wire(transaction, network),
        vjoinsplit,
        value_balance,
        value_balance_zat,
        shielded_spends,
        shielded_outputs,
        orchard,
    }
}

/// The header fields and chain position shared by both `getblock` verbosities
/// (domain -> wire). `size` is the serialized block length the decoded-block read
/// measured; `tx` is supplied by the caller (ids at verbosity 1, decoded
/// transactions at verbosity 2). `previousblockhash` is absent for genesis.
fn block_response<T>(
    block: &Block,
    verbose: &BlockVerbose,
    size: u64,
    tx: Vec<T>,
) -> BlockResponse<T> {
    let header = &block.header;
    let height: u32 = header.height.into();
    BlockResponse {
        hash: to_hex(header.hash.into()),
        confirmations: verbose.confirmations,
        height,
        version: header.version,
        merkle_root: to_hex(header.merkle_root.into()),
        time: header.time,
        nonce: to_hex(header.nonce),
        bits: format!("{:08x}", header.bits.as_bits()),
        difficulty: verbose.difficulty,
        chainwork: verbose
            .chainwork
            .map(|work| bytes_to_hex(&work.to_be_bytes())),
        size,
        previous_block_hash: (height != 0).then(|| to_hex(header.prev_hash.into())),
        next_block_hash: verbose.next_block_hash.map(|hash| to_hex(hash.into())),
        tx,
    }
}

/// Render a block as the `getblock` verbosity-1 response (domain -> wire): the
/// header/position fields, the serialized size, and `tx` as the transaction ids.
pub(crate) fn block_to_wire_v1(
    block: &Block,
    verbose: &BlockVerbose,
    views: &BlockTransactionViews,
) -> BlockResponse<String> {
    let tx = views
        .transactions
        .iter()
        .map(|view| to_hex(view.transaction.txid.into()))
        .collect();
    block_response(block, verbose, views.size, tx)
}

/// Render a block as the `getblock` verbosity-2 response (domain -> wire): the
/// header/position fields, the serialized size, and `tx` as the decoded
/// transactions with every transparent input resolved.
pub(crate) fn block_to_wire_v2(
    block: &Block,
    verbose: &BlockVerbose,
    views: &BlockTransactionViews,
    network: &Network,
) -> BlockResponse<TransactionObject> {
    let tx = views
        .transactions
        .iter()
        .map(|view| transaction_view_to_wire(view, network))
        .collect();
    block_response(block, verbose, views.size, tx)
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
        block_header_to_wire, block_to_wire_v1, block_to_wire_v2, blockchain_info_to_wire,
        signed_zatoshis_to_zec, transaction_view_to_wire, validated_to_wire, z_validated_to_wire,
        zatoshis_to_zec,
    };
    use serde_json::Value;
    use zaino_address::{ValidatedAddress, ZValidatedAddress};
    use zaino_primitives::types::rpc::BlockHeaderVerbose;
    use zaino_primitives::types::{
        AbsoluteChainWork, Block, BlockHash, BlockHeader, BlockTreeSizes, BlockVerbose,
        BlockchainInfo, ChainMetadata, CoinbaseInput, CompactCiphertext, CompactDifficulty,
        ConsensusBranchId, ConsensusBranchIds, EphemeralKey, EquihashSolution, Height,
        JoinSplitValues, NetworkUpgradeInfo, NetworkUpgradeStatus, NoteCommitment, Nullifier,
        OrchardAction, OrchardData, SaplingData, SaplingOutput, SaplingSpend, Script,
        SignedZatoshis, Transaction, TransactionDetail, TransactionId, TransparentData,
        TransparentInput, TransparentOutput, TreeSize, ValuePoolBalance, Zatoshis,
    };
    use zaino_service::{BlockTransactionViews, ResolvedInput, TransactionView};
    use zcash_protocol::consensus::Network;

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
        let json = serde_json::to_value(blockchain_info_to_wire(info)).expect("serialize");
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

    /// The serving network for the transaction golden tests. Mainnet, so a
    /// decoded address is a `t1…`/`t3…`.
    const NET: Network = Network::MainNetwork;

    /// A standard 25-byte P2PKH locking script, so `scriptPubKey` decodes to an
    /// address. The exact base58 string is pinned in `zaino-address`; here only
    /// the shape (one address, `pubkeyhash` type) matters.
    fn p2pkh_script() -> Script {
        let mut bytes = vec![0x76, 0xa9, 0x14];
        bytes.extend_from_slice(&[0x42; 20]);
        bytes.extend_from_slice(&[0x88, 0xac]);
        Script::new(bytes)
    }

    /// A transaction with empty pools and the given id.
    fn empty_transaction(txid: u8) -> Transaction {
        Transaction {
            txid: TransactionId::from([txid; 32]),
            transparent: TransparentData::default(),
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        }
    }

    /// A transaction detail at `version`, overwintered from version 3 with a
    /// distinguishable version group id and a zero expiry (which zcashd still
    /// renders).
    fn base_detail(version: u32) -> TransactionDetail {
        let overwintered = version >= 3;
        TransactionDetail {
            version,
            overwintered,
            version_group_id: overwintered.then_some(0x892f_2085),
            lock_time: 17,
            expiry_height: overwintered.then(|| Height::try_from(0).expect("valid height")),
            size: 211,
            coinbase: None,
            joinsplits: Vec::new(),
        }
    }

    fn view(
        transaction: Transaction,
        detail: TransactionDetail,
        inputs: Vec<ResolvedInput>,
    ) -> TransactionView {
        TransactionView {
            transaction,
            detail,
            inputs,
        }
    }

    /// A 52-byte compact ciphertext head; the explorer never reads it.
    fn ciphertext() -> CompactCiphertext {
        CompactCiphertext::from([0u8; CompactCiphertext::LENGTH])
    }

    /// Coinbase: `vin` is a single `{coinbase: <hex string>, sequence}` built
    /// from the detail, not the block position. Review Focus 1 (fetched alone).
    #[test]
    fn coinbase_transaction_golden_keys() {
        let mut tx = empty_transaction(0xC0);
        tx.transparent.outputs = vec![TransparentOutput {
            value: Zatoshis::new(625_000_000).expect("valid amount"),
            script: p2pkh_script(),
        }];
        let mut detail = base_detail(4);
        detail.coinbase = Some(CoinbaseInput {
            script: Script::new(vec![0x03, 0x01, 0x02, 0x03]),
            sequence: 0xffff_ffff,
        });

        let json = serde_json::to_value(transaction_view_to_wire(
            &view(tx, detail, Vec::new()),
            &NET,
        ))
        .expect("serialize");
        assert_eq!(
            sorted_keys(&json),
            [
                "expiryheight",
                "locktime",
                "overwintered",
                "size",
                "txid",
                "vShieldedOutput",
                "vShieldedSpend",
                "valueBalance",
                "valueBalanceZat",
                "version",
                "versiongroupid",
                "vin",
                "vjoinsplit",
                "vout",
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("version").and_then(Value::as_u64), Some(4));
        assert_eq!(obj.get("overwintered").and_then(Value::as_bool), Some(true));
        assert_eq!(obj.get("locktime").and_then(Value::as_u64), Some(17));
        assert_eq!(obj.get("size").and_then(Value::as_u64), Some(211));
        // Overwinter group id as 8-digit hex.
        assert_eq!(
            obj.get("versiongroupid").and_then(Value::as_str),
            Some("892f2085")
        );
        assert_eq!(obj.get("expiryheight").and_then(Value::as_u64), Some(0));

        let vin = obj.get("vin").and_then(Value::as_array).expect("vin array");
        assert_eq!(vin.len(), 1);
        assert_eq!(sorted_keys(&vin[0]), ["coinbase", "sequence"]);
        let input = vin[0].as_object().expect("an object");
        // `coinbase` is the scriptSig hex (a string), not a bool flag.
        assert_eq!(
            input.get("coinbase").and_then(Value::as_str),
            Some("03010203")
        );
        assert_eq!(
            input.get("sequence").and_then(Value::as_u64),
            Some(0xffff_ffff)
        );

        // The coinbase vout still carries the exact value, the ZEC float, and the
        // decoded address.
        let vout = obj.get("vout").and_then(Value::as_array).expect("vout");
        assert_eq!(
            sorted_keys(&vout[0]),
            ["n", "scriptPubKey", "value", "valueZat"]
        );
        let out = vout[0].as_object().expect("an object");
        assert_eq!(out.get("value").and_then(Value::as_f64), Some(6.25));
        assert_eq!(
            out.get("valueZat").and_then(Value::as_u64),
            Some(625_000_000)
        );
        let spk = out
            .get("scriptPubKey")
            .and_then(Value::as_object)
            .expect("scriptPubKey");
        assert_eq!(
            sorted_keys(&vout[0]["scriptPubKey"]),
            ["addresses", "hex", "type"]
        );
        assert_eq!(spk.get("type").and_then(Value::as_str), Some("pubkeyhash"));
        assert_eq!(
            spk.get("addresses").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
    }

    /// Transparent-only: each spend resolves to the value and address of the
    /// output it spends; the float comes from the shared helper beside its exact
    /// `valueSat`. A non-standard output script omits `addresses`/`type`.
    #[test]
    fn transparent_transaction_golden_keys() {
        let mut tx = empty_transaction(0xAB);
        tx.transparent = TransparentData {
            inputs: vec![TransparentInput {
                prev_txid: TransactionId::from([0x01; 32]),
                prev_index: 3,
            }],
            outputs: vec![
                TransparentOutput {
                    value: Zatoshis::new(2_000).expect("valid amount"),
                    script: p2pkh_script(),
                },
                TransparentOutput {
                    // A bare `OP_RETURN` push — not an address.
                    value: Zatoshis::ZERO,
                    script: Script::new(vec![0x6a, 0x04, 0xde, 0xad, 0xbe, 0xef]),
                },
            ],
        };
        let spent = ResolvedInput {
            outpoint: TransparentInput {
                prev_txid: TransactionId::from([0x01; 32]),
                prev_index: 3,
            },
            spent: TransparentOutput {
                value: Zatoshis::new(150_000_000).expect("valid amount"),
                script: p2pkh_script(),
            },
        };

        let json = serde_json::to_value(transaction_view_to_wire(
            &view(tx, base_detail(4), vec![spent]),
            &NET,
        ))
        .expect("serialize");
        let obj = json.as_object().expect("a JSON object");

        let vin = obj.get("vin").and_then(Value::as_array).expect("vin");
        assert_eq!(
            sorted_keys(&vin[0]),
            ["address", "txid", "value", "valueSat", "vout"]
        );
        let input = vin[0].as_object().expect("an object");
        assert_eq!(
            input.get("txid").and_then(Value::as_str),
            Some("01".repeat(32).as_str())
        );
        assert_eq!(input.get("vout").and_then(Value::as_u64), Some(3));
        // The spent output's value: ZEC float from the shared helper beside the
        // exact zatoshis. 150_000_000 zat = 1.5 ZEC.
        assert_eq!(
            input.get("value").and_then(Value::as_f64),
            Some(zatoshis_to_zec(Zatoshis::new(150_000_000).expect("valid")))
        );
        assert_eq!(input.get("value").and_then(Value::as_f64), Some(1.5));
        assert_eq!(
            input.get("valueSat").and_then(Value::as_u64),
            Some(150_000_000)
        );

        let vout = obj.get("vout").and_then(Value::as_array).expect("vout");
        assert_eq!(vout.len(), 2);
        // n comes from the u32 range, ascending.
        assert_eq!(vout[0]["n"].as_u64(), Some(0));
        assert_eq!(vout[1]["n"].as_u64(), Some(1));
        // The standard output decodes; the OP_RETURN output is hex only.
        assert_eq!(
            sorted_keys(&vout[0]["scriptPubKey"]),
            ["addresses", "hex", "type"]
        );
        assert_eq!(sorted_keys(&vout[1]["scriptPubKey"]), ["hex"]);
        assert!(
            !vout[1]["scriptPubKey"]
                .as_object()
                .expect("object")
                .contains_key("addresses"),
            "a non-standard script is not an address: no addresses key"
        );
    }

    /// Sapling v4: the `valueBalance` family and the `vShielded*` arrays appear;
    /// the explorer reads only their lengths, and `valueBalance` is the signed
    /// ZEC float beside its exact zatoshis.
    #[test]
    fn sapling_v4_transaction_golden_keys() {
        let mut tx = empty_transaction(0x5A);
        tx.sapling = SaplingData {
            spends: vec![SaplingSpend {
                nullifier: Nullifier::from([0x11; 32]),
            }],
            outputs: vec![SaplingOutput {
                cmu: NoteCommitment::from([0x22; 32]),
                ephemeral_key: EphemeralKey::from([0x33; 32]),
                enc_ciphertext: ciphertext(),
            }],
            value_balance: SignedZatoshis::try_new(-200_000_000).expect("valid delta"),
        };

        let json = serde_json::to_value(transaction_view_to_wire(
            &view(tx, base_detail(4), Vec::new()),
            &NET,
        ))
        .expect("serialize");
        let obj = json.as_object().expect("a JSON object");
        assert!(!obj.contains_key("orchard"), "v4 emits no orchard key");

        // -200_000_000 zat = -2.0 ZEC, from the signed helper, beside the exact int.
        assert_eq!(
            obj.get("valueBalance").and_then(Value::as_f64),
            Some(signed_zatoshis_to_zec(
                SignedZatoshis::try_new(-200_000_000).expect("valid")
            ))
        );
        assert_eq!(obj.get("valueBalance").and_then(Value::as_f64), Some(-2.0));
        assert_eq!(
            obj.get("valueBalanceZat").and_then(Value::as_i64),
            Some(-200_000_000)
        );

        let spends = obj
            .get("vShieldedSpend")
            .and_then(Value::as_array)
            .expect("spends");
        assert_eq!(sorted_keys(&spends[0]), ["nullifier"]);
        assert_eq!(
            spends[0]["nullifier"].as_str(),
            Some("11".repeat(32).as_str())
        );
        let outputs = obj
            .get("vShieldedOutput")
            .and_then(Value::as_array)
            .expect("outputs");
        assert_eq!(sorted_keys(&outputs[0]), ["cmu", "ephemeralKey"]);
        assert_eq!(outputs[0]["cmu"].as_str(), Some("22".repeat(32).as_str()));
        assert_eq!(
            outputs[0]["ephemeralKey"].as_str(),
            Some("33".repeat(32).as_str())
        );
    }

    /// Orchard v5 with actions: the `orchard` bundle appears, its `valueBalance`
    /// the signed ZEC float. The Ironwood pool is deliberately not rendered — the
    /// explorer has no field for it (a recorded divergence), so the key is absent
    /// even when the transaction carries Ironwood actions.
    #[test]
    fn orchard_v5_with_actions_golden_keys() {
        let mut tx = empty_transaction(0x05);
        tx.orchard = OrchardData {
            actions: vec![OrchardAction {
                nullifier: Nullifier::from([0xaa; 32]),
                cmx: NoteCommitment::from([0xbb; 32]),
                ephemeral_key: EphemeralKey::from([0xcc; 32]),
                enc_ciphertext: ciphertext(),
            }],
            value_balance: SignedZatoshis::try_new(300_000_000).expect("valid delta"),
        };
        // Ironwood data present in the domain, to prove it is not emitted.
        tx.ironwood = OrchardData {
            actions: vec![OrchardAction {
                nullifier: Nullifier::from([0xde; 32]),
                cmx: NoteCommitment::from([0xad; 32]),
                ephemeral_key: EphemeralKey::from([0xbe; 32]),
                enc_ciphertext: ciphertext(),
            }],
            value_balance: SignedZatoshis::try_new(1).expect("valid delta"),
        };

        let json = serde_json::to_value(transaction_view_to_wire(
            &view(tx, base_detail(5), Vec::new()),
            &NET,
        ))
        .expect("serialize");
        let obj = json.as_object().expect("a JSON object");
        assert!(
            !obj.contains_key("ironwood"),
            "Ironwood is a recorded divergence: never emitted"
        );
        let orchard = obj
            .get("orchard")
            .and_then(Value::as_object)
            .expect("orchard");
        assert_eq!(
            sorted_keys(obj.get("orchard").expect("orchard")),
            ["actions", "valueBalance", "valueBalanceZat"]
        );
        // 300_000_000 zat = 3.0 ZEC.
        assert_eq!(
            orchard.get("valueBalance").and_then(Value::as_f64),
            Some(3.0)
        );
        assert_eq!(
            orchard.get("valueBalanceZat").and_then(Value::as_i64),
            Some(300_000_000)
        );
        let actions = orchard
            .get("actions")
            .and_then(Value::as_array)
            .expect("actions");
        assert_eq!(actions.len(), 1);
        assert_eq!(
            sorted_keys(&actions[0]),
            ["cmx", "ephemeralKey", "nullifier"]
        );
    }

    /// Review Focus 5: a v5 transaction with no Orchard actions still emits the
    /// `orchard` object with an empty `actions` array and `valueBalance: 0.0`,
    /// because the explorer dereferences `orchard.valueBalance` strictly on v5.
    #[test]
    fn v5_without_orchard_actions_still_emits_orchard() {
        let tx = empty_transaction(0x50); // orchard default: no actions, zero balance
        let json = serde_json::to_value(transaction_view_to_wire(
            &view(tx, base_detail(5), Vec::new()),
            &NET,
        ))
        .expect("serialize");
        let obj = json.as_object().expect("a JSON object");
        let orchard = obj
            .get("orchard")
            .and_then(Value::as_object)
            .expect("orchard present on v5");
        assert_eq!(
            orchard
                .get("actions")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0),
            "no actions, but the array is present"
        );
        assert_eq!(
            orchard.get("valueBalance").and_then(Value::as_f64),
            Some(0.0),
            "valueBalance is a float zero, read strictly on v5"
        );
        assert_eq!(
            orchard.get("valueBalanceZat").and_then(Value::as_i64),
            Some(0)
        );
    }

    /// Sprout v2: not overwintered and pre-Sapling, so no envelope or shielded
    /// keys — just the non-empty `vjoinsplit`, each entry's `vpub_*` as the ZEC
    /// float beside its exact zatoshis.
    #[test]
    fn sprout_v2_transaction_golden_keys() {
        let tx = empty_transaction(0x02);
        let mut detail = base_detail(2);
        detail.joinsplits = vec![JoinSplitValues {
            vpub_old: Zatoshis::new(100_000_000).expect("valid amount"),
            vpub_new: Zatoshis::new(50_000_000).expect("valid amount"),
        }];

        let json = serde_json::to_value(transaction_view_to_wire(
            &view(tx, detail, Vec::new()),
            &NET,
        ))
        .expect("serialize");
        assert_eq!(
            sorted_keys(&json),
            [
                "locktime",
                "overwintered",
                "size",
                "txid",
                "version",
                "vin",
                "vjoinsplit",
                "vout",
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("overwintered").and_then(Value::as_bool),
            Some(false)
        );
        let js = obj
            .get("vjoinsplit")
            .and_then(Value::as_array)
            .expect("vjoinsplit");
        assert_eq!(
            sorted_keys(&js[0]),
            ["vpub_new", "vpub_newZat", "vpub_old", "vpub_oldZat"]
        );
        assert_eq!(js[0]["vpub_old"].as_f64(), Some(1.0));
        assert_eq!(js[0]["vpub_oldZat"].as_u64(), Some(100_000_000));
        assert_eq!(js[0]["vpub_new"].as_f64(), Some(0.5));
        assert_eq!(js[0]["vpub_newZat"].as_u64(), Some(50_000_000));
    }

    /// A block header with distinguishable values, for the block golden tests.
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

    /// A block whose own transactions are unused by the renderer (the views carry
    /// them); only its header is read.
    fn scripted_block() -> Block {
        Block {
            header: scripted_block_header(),
            transactions: Vec::new(),
            chain_metadata: ChainMetadata::ZERO,
        }
    }

    /// Chain-position facts for the block golden tests.
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

    /// A coinbase view and a spend view, with a block size, for the block tests.
    fn scripted_views() -> BlockTransactionViews {
        let mut coinbase = empty_transaction(0xC0);
        coinbase.transparent.outputs = vec![TransparentOutput {
            value: Zatoshis::new(625_000_000).expect("valid amount"),
            script: p2pkh_script(),
        }];
        let mut coinbase_detail = base_detail(4);
        coinbase_detail.coinbase = Some(CoinbaseInput {
            script: Script::new(vec![0x03, 0x01, 0x02, 0x03]),
            sequence: 0xffff_ffff,
        });

        let mut spend = empty_transaction(0x7A);
        spend.transparent.inputs = vec![TransparentInput {
            prev_txid: TransactionId::from([0x01; 32]),
            prev_index: 0,
        }];
        let spent = ResolvedInput {
            outpoint: TransparentInput {
                prev_txid: TransactionId::from([0x01; 32]),
                prev_index: 0,
            },
            spent: TransparentOutput {
                value: Zatoshis::new(500).expect("valid amount"),
                script: p2pkh_script(),
            },
        };

        BlockTransactionViews {
            size: 1_234,
            transactions: vec![
                view(coinbase, coinbase_detail, Vec::new()),
                view(spend, base_detail(4), vec![spent]),
            ],
        }
    }

    /// The common block key set shared by both verbosities, including `size`,
    /// `previousblockhash` and `nextblockhash`.
    const BLOCK_KEYS: [&str; 14] = [
        "bits",
        "chainwork",
        "confirmations",
        "difficulty",
        "hash",
        "height",
        "merkleroot",
        "nextblockhash",
        "nonce",
        "previousblockhash",
        "size",
        "time",
        "tx",
        "version",
    ];

    /// Verbosity 1: the header/position keys, `size`, and `tx` as id strings.
    #[test]
    fn block_v1_golden_shape() {
        let views = scripted_views();
        let json = serde_json::to_value(block_to_wire_v1(
            &scripted_block(),
            &scripted_block_verbose(),
            &views,
        ))
        .expect("serialize");
        assert_eq!(sorted_keys(&json), BLOCK_KEYS);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("size").and_then(Value::as_u64), Some(1_234));
        assert_eq!(
            obj.get("previousblockhash").and_then(Value::as_str),
            Some("22".repeat(32).as_str())
        );
        assert_eq!(
            obj.get("nextblockhash").and_then(Value::as_str),
            Some("66".repeat(32).as_str())
        );
        let tx = obj.get("tx").and_then(Value::as_array).expect("tx array");
        assert_eq!(tx.len(), 2);
        // Verbosity 1 lists transaction ids as bare strings.
        assert_eq!(tx[0].as_str(), Some("c0".repeat(32).as_str()));
        assert_eq!(tx[1].as_str(), Some("7a".repeat(32).as_str()));
    }

    /// Verbosity 2: the same keys as verbosity 1, with `tx` as decoded
    /// transactions — the coinbase first, the spend second.
    #[test]
    fn block_v2_golden_shape() {
        let views = scripted_views();
        let json = serde_json::to_value(block_to_wire_v2(
            &scripted_block(),
            &scripted_block_verbose(),
            &views,
            &NET,
        ))
        .expect("serialize");
        assert_eq!(sorted_keys(&json), BLOCK_KEYS);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("size").and_then(Value::as_u64), Some(1_234));
        assert_eq!(obj.get("confirmations").and_then(Value::as_i64), Some(9));
        assert_eq!(
            obj.get("chainwork").and_then(Value::as_str),
            Some(format!("{}deadbeef", "0".repeat(56)).as_str())
        );
        let tx = obj.get("tx").and_then(Value::as_array).expect("tx array");
        assert_eq!(tx.len(), 2);
        // Verbosity 2 renders the decoded transactions: coinbase, then spend.
        let coinbase_vin = tx[0]["vin"].as_array().expect("coinbase vin");
        assert_eq!(sorted_keys(&coinbase_vin[0]), ["coinbase", "sequence"]);
        let spend_vin = tx[1]["vin"].as_array().expect("spend vin");
        assert_eq!(
            sorted_keys(&spend_vin[0]),
            ["address", "txid", "value", "valueSat", "vout"]
        );
    }

    /// Chainwork the validator does not track is absent on the wire, not zero or
    /// null. Pins the `skip_serializing_if` on the block's `chainwork`.
    #[test]
    fn block_response_omits_chainwork_when_untracked() {
        let mut verbose = scripted_block_verbose();
        verbose.chainwork = None;
        let json = serde_json::to_value(block_to_wire_v2(
            &scripted_block(),
            &verbose,
            &scripted_views(),
            &NET,
        ))
        .expect("serialize");
        assert!(
            !json
                .as_object()
                .expect("a JSON object")
                .contains_key("chainwork"),
            "untracked chainwork is omitted, not rendered as zero or null"
        );
    }

    /// Genesis has no previous block, so `previousblockhash` is absent.
    #[test]
    fn genesis_block_omits_previous_block_hash() {
        let mut block = scripted_block();
        block.header.height = Height::try_from(0).expect("genesis height");
        let json = serde_json::to_value(block_to_wire_v1(
            &block,
            &scripted_block_verbose(),
            &scripted_views(),
        ))
        .expect("serialize");
        assert!(
            !json
                .as_object()
                .expect("a JSON object")
                .contains_key("previousblockhash"),
            "genesis has no previous block hash"
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

    /// The ZEC-float renderer is correctly rounded and signed across the amounts
    /// the follow-up tasks reuse it for.
    #[test]
    fn zatoshis_render_as_correctly_rounded_zec() {
        // Zero, the smallest unit, one ZEC, and the supply ceiling.
        assert_eq!(zatoshis_to_zec(Zatoshis::ZERO), 0.0);
        assert_eq!(
            zatoshis_to_zec(Zatoshis::new(1).expect("valid")),
            0.00000001
        );
        assert_eq!(
            zatoshis_to_zec(Zatoshis::new(100_000_000).expect("valid")),
            1.0
        );
        // MAX_MONEY (Zatoshis::MAX) is 21_000_000 ZEC exactly.
        assert_eq!(zatoshis_to_zec(Zatoshis::MAX), 21_000_000.0);

        // A negative delta keeps a single leading sign. -150_000_000 zat = -1.5 ZEC.
        assert_eq!(
            signed_zatoshis_to_zec(SignedZatoshis::try_new(-150_000_000).expect("valid")),
            -1.5
        );

        // Rounding is visible: 0.3 ZEC has no exact f64, so this pins that the
        // division is correctly rounded to the nearest f64 (the `0.3_f64` literal,
        // which prints as 0.3 but is actually 0.299999999999999988…).
        assert_eq!(
            zatoshis_to_zec(Zatoshis::new(30_000_000).expect("valid")),
            0.3_f64
        );
    }

    /// The exact ZEC float of a zatoshi magnitude, by parsing the exact decimal.
    /// This is the reference the infallible split-and-divide must match; parsing
    /// is allowed in test code. `magnitude` is bounded by the caller.
    fn zec_via_decimal(magnitude: u64) -> f64 {
        let whole = magnitude / 100_000_000;
        let frac = magnitude % 100_000_000;
        format!("{whole}.{frac:08}")
            .parse::<f64>()
            .expect("a self-formatted decimal parses")
    }

    /// A bounded magnitude as `i64`. MAX_MONEY is below `i64::MAX`, so this is
    /// exact; the bound is the type invariant, not a runtime guess.
    fn as_bounded_i64(magnitude: u64) -> i64 {
        i64::try_from(magnitude).expect("MAX_MONEY is below i64::MAX")
    }

    /// The split-and-divide helper is bit-identical to parsing the exact decimal,
    /// across the fixed cases and a few hundred pseudo-random bounded amounts. The
    /// generator is a fixed-seed LCG, so the vectors are deterministic and need no
    /// new dependency. The signed renderer agrees on the same magnitude, both
    /// signs.
    #[test]
    fn zec_rendering_matches_the_exact_decimal_parse() {
        let max = Zatoshis::MAX.as_u64();
        let fixed = [0u64, 1, 2_000, 30_000_000, 100_000_000, 21_000_000, max];
        for magnitude in fixed {
            assert_eq!(
                zatoshis_to_zec(Zatoshis::new(magnitude).expect("within supply")),
                zec_via_decimal(magnitude),
                "unsigned magnitude {magnitude}"
            );
        }

        // Numerical Recipes LCG, modulus 2^64 via wrapping arithmetic.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..500 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let magnitude = state % (max + 1);

            let unsigned = Zatoshis::new(magnitude).expect("within supply");
            assert_eq!(
                zatoshis_to_zec(unsigned),
                zec_via_decimal(magnitude),
                "unsigned magnitude {magnitude}"
            );

            let positive =
                SignedZatoshis::try_new(as_bounded_i64(magnitude)).expect("within range");
            assert_eq!(
                signed_zatoshis_to_zec(positive),
                zec_via_decimal(magnitude),
                "positive magnitude {magnitude}"
            );
            let negative =
                SignedZatoshis::try_new(-as_bounded_i64(magnitude)).expect("within range");
            assert_eq!(
                signed_zatoshis_to_zec(negative),
                -zec_via_decimal(magnitude),
                "negative magnitude {magnitude}"
            );
        }
    }
}
