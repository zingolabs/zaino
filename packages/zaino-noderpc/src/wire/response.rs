//! Response bodies, in the field names and encodings zcashd uses.
//!
//! Zatoshi quantities render as integers: `balance` is supply-bounded and fits
//! `u64`, while `received` is a lifetime flow total that is not supply-bounded
//! and so renders from `u128`.

use std::collections::BTreeMap;

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
    /// The same address-kind tag under zcashd's original `type` key. Emitted
    /// alongside `address_type` because the explorer's search pattern-matches on
    /// `type`; carries the identical value.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
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

/// The `getblockheader` response, in zcashd's verbose (`verbose = true`) shape.
///
/// Hashes, the merkle root, the nonce and the Equihash solution are hex; `bits`
/// is the 8-digit hex nBits; `chainwork` is 64-character big-endian hex. The
/// optional hashes and roots are absent rather than `null` when the source does
/// not report them (genesis has no `previousblockhash`; the tip has no
/// `nextblockhash`; pre-Sapling blocks have no `finalsaplingroot`; Zebra reports
/// no `chainwork`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockHeaderResponse {
    /// Hash of this block, as hex.
    pub hash: String,
    /// Depth in the best chain, or `-1` off it. Signed for that reason.
    pub confirmations: i64,
    /// Height of this block.
    pub height: u32,
    /// Header version.
    pub version: u32,
    /// Merkle root of the transaction tree, as hex.
    #[serde(rename = "merkleroot")]
    pub merkle_root: String,
    /// Sapling commitment tree root after this block, as hex. Absent before
    /// Sapling activation and from validators that omit it.
    #[serde(rename = "finalsaplingroot", skip_serializing_if = "Option::is_none")]
    pub final_sapling_root: Option<String>,
    /// Block time, in seconds since the Unix epoch.
    pub time: u32,
    /// Header nonce, as hex.
    pub nonce: String,
    /// Equihash solution, as hex.
    pub solution: String,
    /// Difficulty threshold in compact (nBits) form, as 8-digit hex.
    pub bits: String,
    /// Difficulty as a multiple of the network minimum.
    pub difficulty: f64,
    /// Cumulative chainwork, as 64-character big-endian hex. Absent when the
    /// validator does not track it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chainwork: Option<String>,
    /// Hash of the previous block, as hex. Absent for genesis.
    #[serde(rename = "previousblockhash", skip_serializing_if = "Option::is_none")]
    pub previous_block_hash: Option<String>,
    /// Hash of the next block on the best chain, as hex. Absent for the tip or a
    /// side-chain block.
    #[serde(rename = "nextblockhash", skip_serializing_if = "Option::is_none")]
    pub next_block_hash: Option<String>,
}

/// One input in a verbose transaction's `vin`: either the spend of a previous
/// output, or the block's coinbase input.
///
/// A spend carries `txid` and `vout` only — not the spent output's `address` or
/// `value`. Those name the *spent output*, which lives in an earlier
/// transaction, not in this transaction's own bytes, so resolving them needs a
/// prevout lookup deferred to a follow-up (ruling R37). The coinbase input is a
/// bare marker: the domain drops the coinbase scriptSig, so the hex zcashd
/// reports under `coinbase` is not available here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TransactionInput {
    /// The block's coinbase input — present once, on the coinbase transaction.
    Coinbase {
        /// Always `true`; marks this input as the coinbase.
        coinbase: bool,
    },
    /// A spend of a previous transparent output.
    Spend {
        /// The spent transaction's id, as hex.
        txid: String,
        /// The spent output's index within that transaction.
        vout: u32,
    },
}

/// One output in a verbose transaction's `vout`.
///
/// `valueZat` is the exact zatoshi amount. The ZEC-denominated float zcashd also
/// reports under `value` is omitted, matching the single-source-of-truth choice
/// the chain-info value pools make (`chainValueZat`): only the exact integer
/// crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransactionOutput {
    /// Output value, in zatoshis.
    #[serde(rename = "valueZat")]
    pub value_zat: u64,
    /// Index of this output within the transaction.
    pub n: u32,
}

/// A Sprout JoinSplit description. Uninhabited: Zaino serves no Sprout, so a
/// transaction's `vjoinsplit` array is always empty, which the type enforces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum JoinSplit {}

/// One transaction in a verbose block's `tx` array: its id, inputs, outputs, and
/// (always-empty) Sprout JoinSplits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransactionObject {
    /// Transaction id, as hex.
    pub txid: String,
    /// Inputs — one coinbase marker on the coinbase transaction, spends
    /// otherwise.
    pub vin: Vec<TransactionInput>,
    /// Outputs, in order.
    pub vout: Vec<TransactionOutput>,
    /// Sprout JoinSplits — always empty; Zaino serves no Sprout.
    pub vjoinsplit: Vec<JoinSplit>,
}

/// The `getblock` response at verbosity 2: the block's own header fields, its
/// chain-position facts, and its transactions decoded.
///
/// `size` (the serialized block length) is omitted: re-serializing the block to
/// measure it needs the validator's chain library, which this adapter does not
/// have, so it is a known divergence from zcashd deferred to a follow-up (the
/// same follow-up that resolves input prevouts). The explorer renders a missing
/// `size` as blank rather than failing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockResponse {
    /// Hash of this block, as hex.
    pub hash: String,
    /// Depth in the best chain, or `-1` off it.
    pub confirmations: i64,
    /// Height of this block.
    pub height: u32,
    /// Header version.
    pub version: u32,
    /// Merkle root of the transaction tree, as hex.
    #[serde(rename = "merkleroot")]
    pub merkle_root: String,
    /// Block time, in seconds since the Unix epoch.
    pub time: u32,
    /// Header nonce, as hex.
    pub nonce: String,
    /// Difficulty threshold in compact (nBits) form, as 8-digit hex.
    pub bits: String,
    /// Difficulty as a multiple of the network minimum.
    pub difficulty: f64,
    /// Cumulative chainwork, as 64-character big-endian hex. Absent when the
    /// validator does not track it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chainwork: Option<String>,
    /// The block's transactions, decoded.
    pub tx: Vec<TransactionObject>,
}

/// One value pool in a `getblockchaininfo` response.
///
/// zcashd reports each amount twice — once as a ZEC-denominated float
/// (`chainValue`) and once in zatoshis (`chainValueZat`). Both are emitted: the
/// float is the key clients pattern-match on, and the exact integer travels
/// beside it for callers that need it. The float is lossy by nature; the integer
/// is authoritative.
///
/// The same shape serves the unnamed chain-supply total and the named pools: a
/// pool has an `id`, the total has none, so an empty id is omitted rather than
/// rendered as `""`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValuePoolResponse {
    /// Pool name — `transparent`, `sapling`, `orchard`, … Absent for the
    /// chain-supply total, which zcashd reports unnamed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Whether the validator is tracking this pool's balance.
    pub monitored: bool,
    /// Total value currently in the pool, as a ZEC-denominated float.
    #[serde(rename = "chainValue")]
    pub chain_value: f64,
    /// Total value currently in the pool, in zatoshis — the exact amount.
    #[serde(rename = "chainValueZat")]
    pub chain_value_zat: u64,
    /// Change to the pool's balance from the latest block, as a ZEC float. Absent
    /// when the validator does not report a delta.
    #[serde(rename = "valueDelta", skip_serializing_if = "Option::is_none")]
    pub value_delta: Option<f64>,
    /// Change to the pool's balance from the latest block, in zatoshis — the
    /// exact amount. Absent when the validator does not report a delta; signed,
    /// as value leaves a pool as well as entering it.
    #[serde(rename = "valueDeltaZat", skip_serializing_if = "Option::is_none")]
    pub value_delta_zat: Option<i64>,
}

/// One network upgrade in a `getblockchaininfo` response, keyed in the enclosing
/// map by its consensus branch id (zcashd's layout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkUpgradeResponse {
    /// The validator's descriptive name for the upgrade, e.g. `Canopy`, `NU5`.
    pub name: String,
    /// Height at which the upgrade activates.
    #[serde(rename = "activationheight")]
    pub activation_height: u32,
    /// Status at the validator's current tip: `active`, `pending` or `disabled`.
    pub status: String,
}

/// The consensus branches in force around the tip, as `getblockchaininfo`
/// reports them: 8-digit lowercase hex branch ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TipConsensusResponse {
    /// Branch in force at the current tip.
    pub chaintip: String,
    /// Branch that will be in force for the next block.
    pub nextblock: String,
}

/// The `getblockchaininfo` response, in zcashd's field names and encodings.
///
/// `chainwork` is absent rather than zero when the validator does not track
/// cumulative work (zebra hardcodes it): zero is not a possible amount of work,
/// so the honest wire form is omission.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockchainInfoResponse {
    /// Network name as defined in BIP70 — `main`, `test`, `regtest`.
    pub chain: String,
    /// Number of blocks the validator has fully processed.
    pub blocks: u32,
    /// Height of the best header chain the validator has validated.
    pub headers: u32,
    /// Hash of the current best block, as hex.
    #[serde(rename = "bestblockhash")]
    pub best_block_hash: String,
    /// Current difficulty, as a multiple of the network minimum.
    pub difficulty: f64,
    /// Verification progress relative to the estimated network tip, in `0.0..=1.0`.
    #[serde(rename = "verificationprogress")]
    pub verification_progress: f64,
    /// Total work in the best chain, as 64-character big-endian hex. Absent when
    /// the validator does not track it.
    #[serde(rename = "chainwork", skip_serializing_if = "Option::is_none")]
    pub chain_work: Option<String>,
    /// Whether the validator has pruned block data.
    pub pruned: bool,
    /// Approximate on-disk size of the validator's block and undo data, in bytes.
    pub size_on_disk: u64,
    /// Total note commitments across the shielded pools.
    pub commitments: u64,
    /// Height the validator estimates the network tip to be at.
    #[serde(rename = "estimatedheight")]
    pub estimated_height: u32,
    /// Total transparent and shielded value on the chain, unnamed.
    #[serde(rename = "chainSupply")]
    pub chain_supply: ValuePoolResponse,
    /// Per-pool value balances.
    #[serde(rename = "valuePools")]
    pub value_pools: Vec<ValuePoolResponse>,
    /// Network upgrade schedule, keyed by 8-digit lowercase hex branch id.
    pub upgrades: BTreeMap<String, NetworkUpgradeResponse>,
    /// Consensus branches in force at the tip and for the next block.
    pub consensus: TipConsensusResponse,
}

#[cfg(test)]
mod tests {
    use super::{
        AddressBalanceResponse, AddressDeltaEntry, AddressDeltasResponse, DeltaRange,
        UnifiedReceiversResponse, ValidateAddressResponse,
    };
    use serde_json::Value;

    /// The sorted key set of a JSON object, for pinning the exact wire shape:
    /// an extra key fails the comparison as surely as a missing one.
    fn sorted_keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("response serializes to a JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    fn to_value<T: serde::Serialize>(value: &T) -> Value {
        serde_json::to_value(value).expect("response serializes")
    }

    /// `getaddressbalance` carries exactly `balance` and `received`, both as JSON
    /// numbers. Fails if a key is renamed/added/dropped or a quantity is rendered
    /// as a string instead of a number.
    #[test]
    fn address_balance_response_golden_shape() {
        let json = to_value(&AddressBalanceResponse {
            balance: 5_000,
            received: 12_000,
        });
        assert_eq!(sorted_keys(&json), ["balance", "received"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("balance").and_then(Value::as_u64), Some(5_000));
        assert_eq!(obj.get("received").and_then(Value::as_u64), Some(12_000));
    }

    /// One delta entry with a known block index carries all six keys, with
    /// `block_index` rendered under the zcashd spelling `blockindex`. Fails if a
    /// key is renamed/added/dropped or a number is rendered as a string.
    #[test]
    fn address_delta_entry_with_block_index_golden_shape() {
        let json = to_value(&AddressDeltaEntry {
            satoshis: -3,
            txid: "aa".repeat(32),
            index: 2,
            block_index: Some(1),
            height: 150,
            address: "t1a".to_string(),
        });
        assert_eq!(
            sorted_keys(&json),
            [
                "address",
                "blockindex",
                "height",
                "index",
                "satoshis",
                "txid"
            ]
        );
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("satoshis").and_then(Value::as_i64), Some(-3));
        assert_eq!(
            obj.get("txid").and_then(Value::as_str),
            Some("aa".repeat(32).as_str())
        );
        assert_eq!(obj.get("index").and_then(Value::as_u64), Some(2));
        assert_eq!(obj.get("blockindex").and_then(Value::as_u64), Some(1));
        assert_eq!(obj.get("height").and_then(Value::as_u64), Some(150));
        assert_eq!(obj.get("address").and_then(Value::as_str), Some("t1a"));
    }

    /// Documented divergence from zcashd, pinned to *our* shape: zcashd always
    /// emits `blockindex`, whereas Zaino omits it when the source cannot supply
    /// the in-block position. Recorded in the spec
    /// (`zaino-design/design/explorer-grant-completion.md`, "Known wire
    /// divergences"). Do not "fix" this to match zcashd — the omission is the
    /// honest answer when the position is unknown. `blockindex` must be *absent*,
    /// not `null`, so `skip_serializing_if` is pinned here.
    #[test]
    fn address_delta_entry_omits_block_index_when_absent() {
        let json = to_value(&AddressDeltaEntry {
            satoshis: 7,
            txid: "bb".repeat(32),
            index: 0,
            block_index: None,
            height: 10,
            address: "t1b".to_string(),
        });
        assert_eq!(
            sorted_keys(&json),
            ["address", "height", "index", "satoshis", "txid"]
        );
        let obj = json.as_object().expect("a JSON object");
        assert!(
            !obj.contains_key("blockindex"),
            "an absent block index is omitted, not rendered as null"
        );
    }

    /// Documented divergence from zcashd, pinned to *our* shape: zcashd returns
    /// top-level `start` and `end` objects each `{hash, height}`, whereas Zaino
    /// wraps one `range` object of bare heights. Recorded in the spec
    /// (`zaino-design/design/explorer-grant-completion.md`, "Known wire
    /// divergences"). Do not "fix" this test to zcashd's shape — the wrapper is
    /// intentional.
    #[test]
    fn address_deltas_response_with_range_golden_shape() {
        let json = to_value(&AddressDeltasResponse {
            deltas: vec![AddressDeltaEntry {
                satoshis: -3,
                txid: "aa".repeat(32),
                index: 0,
                block_index: Some(1),
                height: 150,
                address: "t1a".to_string(),
            }],
            range: Some(DeltaRange {
                start: 100,
                end: 200,
            }),
        });
        assert_eq!(sorted_keys(&json), ["deltas", "range"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("deltas").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
        let range = obj.get("range").expect("range present");
        assert_eq!(sorted_keys(range), ["end", "start"]);
        let range = range.as_object().expect("range is an object");
        assert_eq!(range.get("start").and_then(Value::as_u64), Some(100));
        assert_eq!(range.get("end").and_then(Value::as_u64), Some(200));
    }

    /// Without `chainInfo`, the response is the bare delta list — `range` is
    /// absent, not `null`. Pins the `skip_serializing_if` on `range`.
    #[test]
    fn address_deltas_response_without_range_omits_it() {
        let json = to_value(&AddressDeltasResponse {
            deltas: Vec::new(),
            range: None,
        });
        assert_eq!(sorted_keys(&json), ["deltas"]);
        assert!(
            !json
                .as_object()
                .expect("a JSON object")
                .contains_key("range"),
            "an absent range is omitted, not rendered as null"
        );
    }

    /// A valid transparent address carries `isvalid`, `address` and `isscript`.
    /// Fails if a key is renamed/added/dropped or a type changes.
    #[test]
    fn validate_address_response_valid_golden_shape() {
        let json = to_value(&ValidateAddressResponse {
            isvalid: true,
            address: Some("t1abc".to_string()),
            isscript: Some(false),
        });
        assert_eq!(sorted_keys(&json), ["address", "isscript", "isvalid"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("isvalid").and_then(Value::as_bool), Some(true));
        assert_eq!(obj.get("address").and_then(Value::as_str), Some("t1abc"));
        assert_eq!(obj.get("isscript").and_then(Value::as_bool), Some(false));
    }

    /// An invalid address reports only `isvalid: false`; the two optional keys
    /// are absent, not `null`. Pins their `skip_serializing_if`.
    #[test]
    fn validate_address_response_invalid_golden_shape() {
        let json = to_value(&ValidateAddressResponse {
            isvalid: false,
            address: None,
            isscript: None,
        });
        assert_eq!(sorted_keys(&json), ["isvalid"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(obj.get("isvalid").and_then(Value::as_bool), Some(false));
        assert!(!obj.contains_key("address"));
        assert!(!obj.contains_key("isscript"));
    }

    /// A unified address bundling every receiver kind renders all four keys.
    /// Fails if a key is renamed/added/dropped or a value's type changes.
    #[test]
    fn unified_receivers_response_all_present_golden_shape() {
        let json = to_value(&UnifiedReceiversResponse {
            orchard: Some("u1orchard".to_string()),
            sapling: Some("zs1sapling".to_string()),
            p2pkh: Some("t1pkh".to_string()),
            p2sh: Some("t3sh".to_string()),
        });
        assert_eq!(sorted_keys(&json), ["orchard", "p2pkh", "p2sh", "sapling"]);
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("orchard").and_then(Value::as_str),
            Some("u1orchard")
        );
        assert_eq!(
            obj.get("sapling").and_then(Value::as_str),
            Some("zs1sapling")
        );
        assert_eq!(obj.get("p2pkh").and_then(Value::as_str), Some("t1pkh"));
        assert_eq!(obj.get("p2sh").and_then(Value::as_str), Some("t3sh"));
    }

    /// Receiver kinds the address does not carry are omitted, not `null`. Pins
    /// the per-field `skip_serializing_if`.
    #[test]
    fn unified_receivers_response_partly_absent_golden_shape() {
        let json = to_value(&UnifiedReceiversResponse {
            orchard: Some("u1orchard".to_string()),
            sapling: None,
            p2pkh: Some("t1pkh".to_string()),
            p2sh: None,
        });
        assert_eq!(sorted_keys(&json), ["orchard", "p2pkh"]);
        let obj = json.as_object().expect("a JSON object");
        assert!(!obj.contains_key("sapling"));
        assert!(!obj.contains_key("p2sh"));
    }
}
