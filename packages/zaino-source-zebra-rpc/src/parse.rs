//! Response parsing: JSON-RPC `serde_json::Value` → zaino-primitives types.
//!
//! Each function corresponds to one RPC method's response format as
//! returned by Zebra.
//!
//! # When to be lenient
//!
//! This backend must work against any validator serving the Zcash JSON-RPC
//! interface, so it cannot fail merely because a response is unfamiliar. But
//! leniency is not uniform, and the rule is what a wrong value would *do*:
//!
//! - **Lenient where the value is informational.** An unrecognised
//!   `getchaintips` status becomes [`ChainTipStatus::Unknown`]. The tip still
//!   exists and the caller still learns of it; discarding the whole listing
//!   over one unfamiliar label would lose far more than it protects.
//! - **Strict where Zaino acts on the value.** An unrecognised network upgrade
//!   status fails the parse outright, because Zaino adopts the upgrade schedule
//!   from `getblockchaininfo` as its activation heights. Skipping an entry we
//!   did not understand would leave Zaino running consensus rules its validator
//!   is not, and a silently short schedule is far worse than a loud failure.
//!
//! The same rule governs absence. A missing optional field is `None`, but a
//! field that is present and malformed is an error: "the pool is not active"
//! and "the response is garbled" are different facts, and conflating them makes
//! a broken validator look like a pre-activation block.

use incrementalmerkletree::frontier::CommitmentTree;
use zaino_primitives::types::{
    rpc::{
        BlockDelta, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, ChainTip, ChainTipStatus,
        FundingStream, InputDelta, LockboxStream, MiningInfo, NodeInfo, OutputDelta, PeerInfo,
        ScriptPubKey, SpentInfo, TxOut,
    },
    AddressBalance, AddressDelta, BlockCommitments, BlockHash, BlockTreeSizes, BlockVerbose,
    BlockchainInfo, ChainWork, ConsensusBranchId, ConsensusBranchIds, Height, MerkleRoot,
    NetworkUpgradeInfo, NetworkUpgradeStatus, Script, SignedZatoshis, SubtreeRoot, TransactionId,
    TransactionLocation, TransparentAddress, TreeRoot, TreeRootInfo, TreeRoots, Treestate, Utxo,
    ValuePoolBalance, Zatoshis,
};
use zaino_source::{MempoolTxMeta, TransactionResponse};

// ---------------------------------------------------------------------------
// Scalar helpers
// ---------------------------------------------------------------------------

/// Read a required field, failing with the field's name rather than the whole
/// object — an error naming `"confirmations"` is actionable where one quoting
/// 64 characters of JSON is not.
pub(crate) fn field<'a>(
    value: &'a serde_json::Value,
    name: &'static str,
) -> Result<&'a serde_json::Value, ParseError> {
    value.get(name).ok_or(ParseError::MissingField(name))
}

/// Read an optional field, treating JSON `null` as absent.
///
/// Validators express "not applicable" both by omitting a key and by sending
/// it as `null`; a consumer cannot act on the difference, so both become `None`.
pub(crate) fn opt_field<'a>(
    value: &'a serde_json::Value,
    name: &'static str,
) -> Option<&'a serde_json::Value> {
    value.get(name).filter(|v| !v.is_null())
}

pub(crate) fn as_str(value: &serde_json::Value) -> Result<&str, ParseError> {
    value
        .as_str()
        .ok_or_else(|| ParseError::unexpected("string", value))
}

pub(crate) fn as_u64(value: &serde_json::Value) -> Result<u64, ParseError> {
    value
        .as_u64()
        .ok_or_else(|| ParseError::unexpected("u64", value))
}

pub(crate) fn as_u32(value: &serde_json::Value) -> Result<u32, ParseError> {
    let n = as_u64(value)?;
    u32::try_from(n).map_err(|_| ParseError::Overflow(n))
}

pub(crate) fn as_i64(value: &serde_json::Value) -> Result<i64, ParseError> {
    value
        .as_i64()
        .ok_or_else(|| ParseError::unexpected("i64", value))
}

pub(crate) fn as_f64(value: &serde_json::Value) -> Result<f64, ParseError> {
    value
        .as_f64()
        .ok_or_else(|| ParseError::unexpected("f64", value))
}

pub(crate) fn as_bool(value: &serde_json::Value) -> Result<bool, ParseError> {
    value
        .as_bool()
        .ok_or_else(|| ParseError::unexpected("bool", value))
}

pub(crate) fn as_height(value: &serde_json::Value) -> Result<Height, ParseError> {
    let h = as_u32(value)?;
    Height::try_from(h).map_err(|e| ParseError::Height(e.to_string()))
}

// ---------------------------------------------------------------------------
// 32-byte values
//
// This interface writes some 32-byte values byte-reversed and others in their
// natural order, and both are 64 hex characters, so nothing but knowing the
// field distinguishes them. Choosing wrongly does not fail — it yields a
// silently mirrored value.
//
// So no parser below decodes raw bytes and picks an order. Each *domain type*
// gets one constructor here with its order baked in, and call sites name the
// type they want. `reversed` and `natural` are private to this section and are
// never called from a response parser.
//
// Verified against zebra's own serde (`zebra-rpc`'s `BlockHeaderObject`):
// `block::Hash` and `merkle::Root` reverse on decode, whereas the plain
// `[u8; 32]` fields — blockcommitments, finalsaplingroot, nonce — do not.
// ---------------------------------------------------------------------------

/// Decode a 32-byte value written byte-reversed (RPC display order).
fn reversed(value: &serde_json::Value) -> Result<[u8; 32], ParseError> {
    let bytes = hex::decode(as_str(value)?).map_err(|e| ParseError::Hex(e.to_string()))?;
    let mut le: [u8; 32] = bytes
        .try_into()
        .map_err(|b: Vec<u8>| ParseError::WrongLength {
            expected: 32,
            got: b.len(),
        })?;
    le.reverse();
    Ok(le)
}

/// A transaction id. Reversed on the wire.
pub(crate) fn as_txid(value: &serde_json::Value) -> Result<TransactionId, ParseError> {
    reversed(value).map(TransactionId::from)
}

/// A transaction merkle root. Reversed on the wire, like a hash.
fn as_merkle_root(value: &serde_json::Value) -> Result<MerkleRoot, ParseError> {
    reversed(value).map(MerkleRoot::from)
}

/// A block commitments digest. Natural order.
fn as_block_commitments(value: &serde_json::Value) -> Result<BlockCommitments, ParseError> {
    natural(value).map(BlockCommitments::from)
}

/// A commitment tree root. Natural order.
fn as_tree_root(value: &serde_json::Value) -> Result<TreeRoot, ParseError> {
    natural(value).map(TreeRoot::new)
}

/// An Equihash nonce. Natural order.
fn as_nonce(value: &serde_json::Value) -> Result<[u8; 32], ParseError> {
    natural(value)
}

/// Convert a ZEC-denominated amount to exact zatoshis.
///
/// Amounts cross this interface as JSON floats, which cannot represent every
/// zatoshi value exactly. Rounding to the nearest zatoshi recovers the intended
/// integer for every amount within the money supply — at 21e6 ZEC the zatoshi
/// count is ~2.1e15, comfortably inside f64's 2^53 exact-integer range — so the
/// only error a round can introduce would need the validator to have sent a
/// value that is already wrong.
///
/// Prefer [`zatoshis_field`], which uses the integer field when the response
/// carries one, over calling this on a float.
pub(crate) fn zec_to_zatoshis(zec: f64) -> Result<Zatoshis, ParseError> {
    if !zec.is_finite() || zec < 0.0 {
        return Err(ParseError::Amount(format!("not a ZEC amount: {zec}")));
    }
    let zats = (zec * 1e8).round();
    if zats > u64::MAX as f64 {
        return Err(ParseError::Amount(format!(
            "ZEC amount out of range: {zec}"
        )));
    }
    Zatoshis::new(zats as u64).map_err(|e| ParseError::Amount(e.to_string()))
}

/// Read an amount, preferring an exact zatoshi field over its ZEC counterpart.
///
/// Several responses report the same amount twice — `value` in ZEC and
/// `valueZat` in zatoshis. Reading the integer avoids the float entirely; the
/// ZEC field is the fallback for validators that send only that.
pub(crate) fn zatoshis_field(
    value: &serde_json::Value,
    zat_name: &'static str,
    zec_name: &'static str,
) -> Result<Zatoshis, ParseError> {
    match opt_field(value, zat_name) {
        Some(v) => Zatoshis::new(as_u64(v)?).map_err(|e| ParseError::Amount(e.to_string())),
        None => zec_to_zatoshis(as_f64(field(value, zec_name)?)?),
    }
}

/// Parse a `getblock(height, 0)` response — hex-encoded raw block bytes.
pub(crate) fn parse_raw_block(value: &serde_json::Value) -> Result<Vec<u8>, ParseError> {
    let hex_str = value
        .as_str()
        .ok_or_else(|| ParseError::unexpected("string", value))?;
    hex::decode(hex_str).map_err(|e| ParseError::Hex(e.to_string()))
}

/// A block hash. Reversed on the wire — also the whole of a
/// `getbestblockhash` response.
pub(crate) fn parse_block_hash(value: &serde_json::Value) -> Result<BlockHash, ParseError> {
    reversed(value).map(BlockHash::from)
}

/// Parse a `getblockcount` response — integer height.
pub(crate) fn parse_height(value: &serde_json::Value) -> Result<Height, ParseError> {
    as_height(value)
}

/// Parse one pool's serialised commitment tree out of a `z_gettreestate`
/// response.
///
/// `Ok(None)` means the response carries no tree for this pool — the pool is
/// not active at this height. A tree that is present but not a hex string is a
/// malformed response and errors, rather than being reported as an inactive
/// pool: those are different facts, and conflating them would make a garbled
/// response indistinguishable from a pre-activation block.
fn parse_pool_final_state(
    value: &serde_json::Value,
    pool: &str,
) -> Result<Option<zaino_primitives::types::PoolTreestate>, ParseError> {
    value
        .get(pool)
        .and_then(|p| p.get("commitments"))
        .and_then(|c| c.get("finalState"))
        .map(|v| {
            v.as_str()
                .ok_or_else(|| ParseError::unexpected("string", v))
                .and_then(|hex_str| {
                    hex::decode(hex_str).map_err(|e| ParseError::Hex(e.to_string()))
                })
                .map(|final_state| zaino_primitives::types::PoolTreestate {
                    // `finalRoot` is not read back from the validator's reply.
                    // Zebra's own type documents the field as unused, so
                    // trusting it here would make the answer depend on which
                    // validator is behind the adapter. Roots come from
                    // `get_commitment_tree_roots`, which every adapter answers.
                    final_root: None,
                    final_state,
                })
        })
        .transpose()
}

/// Parse a `z_gettreestate` response.
pub(crate) fn parse_treestate(value: &serde_json::Value) -> Result<Treestate, ParseError> {
    Ok(Treestate {
        block_hash: parse_block_hash(field(value, "hash")?)?,
        height: as_height(field(value, "height")?)?,
        time: as_u32(field(value, "time")?)?,
        sapling: parse_pool_final_state(value, "sapling")?,
        orchard: parse_pool_final_state(value, "orchard")?,
        ironwood: parse_pool_final_state(value, "ironwood")?,
    })
}

/// Errors from parsing RPC responses.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    /// Hex decoding failed.
    #[error("hex decode: {0}")]
    Hex(String),

    /// Unexpected JSON type.
    #[error("expected {expected}, got {got}")]
    UnexpectedType {
        /// What we expected.
        expected: &'static str,
        /// What we got (truncated).
        got: String,
    },

    /// Byte array wrong length.
    #[error("expected {expected} bytes, got {got}")]
    WrongLength {
        /// Expected length.
        expected: usize,
        /// Actual length.
        got: usize,
    },

    /// Value too large.
    #[error("value {0} overflows target type")]
    Overflow(u64),

    /// Height validation failed.
    #[error("invalid height: {0}")]
    Height(String),

    /// A required field was absent from the response.
    #[error("missing field `{0}`")]
    MissingField(&'static str),

    /// A monetary amount was invalid or out of range.
    #[error("invalid amount: {0}")]
    Amount(String),

    /// Block deserialization failed.
    #[error("deserialize: {0}")]
    Deserialize(String),

    /// A mempool listing declared more entries than we are willing to decode.
    #[error("{kind} mempool listing too large: {len} entries > {max}")]
    ListingTooLarge {
        /// Which listing — `"txid"` or `"verbose"`.
        kind: &'static str,
        /// The entry count the validator sent.
        len: usize,
        /// The cap that was exceeded.
        max: usize,
    },
}

impl ParseError {
    fn unexpected(expected: &'static str, value: &serde_json::Value) -> Self {
        let got = format!("{value}").chars().take(64).collect();
        Self::UnexpectedType { expected, got }
    }
}

// ---------------------------------------------------------------------------
// Response parsers
// ---------------------------------------------------------------------------

/// Parse a `getchaintips` response.
pub(crate) fn parse_chain_tips(value: &serde_json::Value) -> Result<Vec<ChainTip>, ParseError> {
    as_array(value)?
        .iter()
        .map(|tip| {
            Ok(ChainTip {
                height: as_height(field(tip, "height")?)?,
                hash: parse_block_hash(field(tip, "hash")?)?,
                branch_len: opt_field(tip, "branchlen")
                    .map(as_u32)
                    .transpose()?
                    .unwrap_or(0),
                status: parse_chain_tip_status(opt_field(tip, "status")),
            })
        })
        .collect()
}

/// Map a `getchaintips` status string onto the interface's vocabulary.
///
/// An unrecognised or absent status becomes [`ChainTipStatus::Unknown`] rather
/// than an error: a validator reporting a status this interface does not define
/// is still telling us a tip exists, and losing the whole listing over one
/// unfamiliar label would be a poor trade.
fn parse_chain_tip_status(value: Option<&serde_json::Value>) -> ChainTipStatus {
    match value.and_then(|v| v.as_str()) {
        Some("active") => ChainTipStatus::Active,
        Some("valid-fork") => ChainTipStatus::ValidFork,
        Some("valid-headers") => ChainTipStatus::ValidHeaders,
        Some("headers-only") => ChainTipStatus::HeadersOnly,
        Some("invalid") => ChainTipStatus::Invalid,
        _ => ChainTipStatus::Unknown,
    }
}

/// Parse a verbose `getblockheader` response.
pub(crate) fn parse_block_header_verbose(
    value: &serde_json::Value,
) -> Result<BlockHeaderVerbose, ParseError> {
    Ok(BlockHeaderVerbose {
        hash: parse_block_hash(field(value, "hash")?)?,
        confirmations: as_i64(field(value, "confirmations")?)?,
        height: as_height(field(value, "height")?)?,
        version: as_u32(field(value, "version")?)?,
        merkle_root: as_merkle_root(field(value, "merkleroot")?)?,
        time: as_u32(field(value, "time")?)?,
        nonce: as_nonce(field(value, "nonce")?)?,
        solution: opt_field(value, "solution")
            .map(|v| hex::decode(as_str(v)?).map_err(|e| ParseError::Hex(e.to_string())))
            .transpose()?
            .unwrap_or_default(),
        bits: parse_compact_difficulty(field(value, "bits")?)?,
        difficulty: as_f64(field(value, "difficulty")?)?,
        block_commitments: opt_field(value, "blockcommitments")
            .map(as_block_commitments)
            .transpose()?,
        final_sapling_root: opt_field(value, "finalsaplingroot")
            .map(as_tree_root)
            .transpose()?,
        chainwork: opt_field(value, "chainwork")
            .map(as_chain_work_natural)
            .transpose()?,
        previous_block_hash: opt_field(value, "previousblockhash")
            .map(parse_block_hash)
            .transpose()?,
        next_block_hash: opt_field(value, "nextblockhash")
            .map(parse_block_hash)
            .transpose()?,
    })
}

/// Parse the compact difficulty (`nBits`), which crosses the wire as hex.
fn parse_compact_difficulty(value: &serde_json::Value) -> Result<u32, ParseError> {
    let s = as_str(value)?;
    u32::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
        .map_err(|e| ParseError::Hex(format!("nBits `{s}`: {e}")))
}

/// Decode a 32-byte value written in its natural order.
fn natural(value: &serde_json::Value) -> Result<[u8; 32], ParseError> {
    let bytes = hex::decode(as_str(value)?).map_err(|e| ParseError::Hex(e.to_string()))?;
    bytes
        .try_into()
        .map_err(|b: Vec<u8>| ParseError::WrongLength {
            expected: 32,
            got: b.len(),
        })
}

/// Parse a `getinfo` response.
pub(crate) fn parse_node_info(value: &serde_json::Value) -> Result<NodeInfo, ParseError> {
    let errors = parse_health_sentinel(opt_field(value, "errors"), "no errors")?;
    Ok(NodeInfo {
        version: as_u64(field(value, "version")?)?,
        build: as_str(field(value, "build")?)?.to_owned(),
        subversion: as_str(field(value, "subversion")?)?.to_owned(),
        protocol_version: as_u32(field(value, "protocolversion")?)?,
        blocks: as_height(field(value, "blocks")?)?,
        connections: as_u64(field(value, "connections")?)?,
        difficulty: as_f64(field(value, "difficulty")?)?,
        testnet: as_bool(field(value, "testnet")?)?,
        proxy: opt_field(value, "proxy")
            .map(|v| as_str(v).map(str::to_owned))
            .transpose()?,
        pay_tx_fee: zec_to_zatoshis(as_f64(field(value, "paytxfee")?)?)?,
        relay_fee: zec_to_zatoshis(as_f64(field(value, "relayfee")?)?)?,
        // The timestamp only means anything alongside a message; without one it
        // is a sentinel describing nothing.
        errors_timestamp: match errors {
            Some(_) => opt_field(value, "errorstimestamp")
                .map(as_i64)
                .transpose()?,
            None => None,
        },
        errors,
    })
}

/// Normalise a health field that signals "nothing wrong" with a sentinel value.
///
/// This interface reports health inconsistently — `getinfo` sends the literal
/// `"no errors"`, `getmininginfo` an empty string — so each caller passes its
/// own sentinel and consumers get a uniform `Option`.
fn parse_health_sentinel(
    value: Option<&serde_json::Value>,
    sentinel: &str,
) -> Result<Option<String>, ParseError> {
    let Some(value) = value else { return Ok(None) };
    let message = as_str(value)?;
    Ok((!message.is_empty() && message != sentinel).then(|| message.to_owned()))
}

/// Parse a `getmininginfo` response.
pub(crate) fn parse_mining_info(value: &serde_json::Value) -> Result<MiningInfo, ParseError> {
    Ok(MiningInfo {
        tip_height: as_height(field(value, "blocks")?)?,
        chain: opt_field(value, "chain")
            .map(|v| as_str(v).map(str::to_owned))
            .transpose()?
            .unwrap_or_default(),
        testnet: opt_field(value, "testnet")
            .map(as_bool)
            .transpose()?
            .unwrap_or(false),
        current_block_size: opt_field(value, "currentblocksize")
            .map(as_u64)
            .transpose()?,
        current_block_tx: opt_field(value, "currentblocktx").map(as_u64).transpose()?,
        network_solution_rate: opt_field(value, "networksolps").map(as_u64).transpose()?,
        network_hash_rate: opt_field(value, "networkhashps").map(as_u64).transpose()?,
        difficulty: opt_field(value, "difficulty").map(as_f64).transpose()?,
        errors: parse_health_sentinel(opt_field(value, "errors"), "")?,
    })
}

/// Parse a `getpeerinfo` response.
pub(crate) fn parse_peer_info(value: &serde_json::Value) -> Result<Vec<PeerInfo>, ParseError> {
    as_array(value)?
        .iter()
        .map(|peer| {
            Ok(PeerInfo {
                addr: as_str(field(peer, "addr")?)?.to_owned(),
                inbound: as_bool(field(peer, "inbound")?)?,
            })
        })
        .collect()
}

pub(crate) fn as_array(value: &serde_json::Value) -> Result<&Vec<serde_json::Value>, ParseError> {
    value
        .as_array()
        .ok_or_else(|| ParseError::unexpected("array", value))
}

/// Parse a `getblocksubsidy` response.
pub(crate) fn parse_block_subsidy(value: &serde_json::Value) -> Result<BlockSubsidy, ParseError> {
    Ok(BlockSubsidy {
        miner: zatoshis_field(value, "minerZat", "miner")?,
        founders: zatoshis_field(value, "foundersZat", "founders")?,
        funding_streams_total: zatoshis_field(
            value,
            "fundingstreamstotalZat",
            "fundingstreamstotal",
        )?,
        lockbox_total: zatoshis_field(value, "lockboxtotalZat", "lockboxtotal")?,
        total_block_subsidy: zatoshis_field(value, "totalblocksubsidyZat", "totalblocksubsidy")?,
        funding_streams: parse_optional_list(value, "fundingstreams", |s| {
            Ok(FundingStream {
                recipient: as_str(field(s, "recipient")?)?.to_owned(),
                specification: as_str(field(s, "specification")?)?.to_owned(),
                value: zatoshis_field(s, "valueZat", "value")?,
                address: opt_field(s, "address")
                    .map(|v| as_str(v).map(str::to_owned))
                    .transpose()?,
            })
        })?,
        lockbox_streams: parse_optional_list(value, "lockboxstreams", |s| {
            Ok(LockboxStream {
                recipient: as_str(field(s, "recipient")?)?.to_owned(),
                specification: as_str(field(s, "specification")?)?.to_owned(),
                value: zatoshis_field(s, "valueZat", "value")?,
            })
        })?,
    })
}

/// Parse a list field that is omitted entirely when it would be empty.
///
/// Several responses drop a list rather than sending `[]`. An absent list and
/// an empty one say the same thing here — nothing of that kind is active — so
/// both yield an empty `Vec` rather than an `Option`.
fn parse_optional_list<T>(
    value: &serde_json::Value,
    name: &'static str,
    mut parse_item: impl FnMut(&serde_json::Value) -> Result<T, ParseError>,
) -> Result<Vec<T>, ParseError> {
    match opt_field(value, name) {
        Some(list) => as_array(list)?.iter().map(&mut parse_item).collect(),
        None => Ok(Vec::new()),
    }
}

/// Parse a `gettxout` response.
///
/// `Ok(None)` is the answer for a spent or unknown outpoint: the validator
/// replies with JSON `null`, which is a real answer to "is this unspent?"
/// rather than a failure.
pub(crate) fn parse_tx_out(value: &serde_json::Value) -> Result<Option<TxOut>, ParseError> {
    if value.is_null() {
        return Ok(None);
    }
    let script = field(value, "scriptPubKey")?;
    Ok(Some(TxOut {
        best_block: parse_block_hash(field(value, "bestblock")?)?,
        confirmations: as_i64(field(value, "confirmations")?)?,
        value: zatoshis_field(value, "valueZat", "value")?,
        coinbase: opt_field(value, "coinbase")
            .map(as_bool)
            .transpose()?
            .unwrap_or(false),
        script_pub_key: ScriptPubKey {
            script: Script::new(
                hex::decode(as_str(field(script, "hex")?)?)
                    .map_err(|e| ParseError::Hex(e.to_string()))?,
            ),
            asm: opt_field(script, "asm")
                .map(|v| as_str(v).map(str::to_owned))
                .transpose()?,
            script_type: opt_field(script, "type")
                .map(|v| as_str(v).map(str::to_owned))
                .transpose()?,
            required_signatures: opt_field(script, "reqSigs").map(as_u32).transpose()?,
            addresses: parse_optional_list(script, "addresses", |a| {
                Ok(TransparentAddress::new(as_str(a)?.to_owned()))
            })?,
        },
    }))
}

/// Parse a `getspentinfo` response.
///
/// `Ok(None)` means the output is unspent or unknown to the validator.
pub(crate) fn parse_spent_info(value: &serde_json::Value) -> Result<Option<SpentInfo>, ParseError> {
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(SpentInfo {
        txid: as_txid(field(value, "txid")?)?,
        index: as_u32(field(value, "index")?)?,
        height: as_height(field(value, "height")?)?,
    }))
}

/// Parse a `getaddressbalance` response.
pub(crate) fn parse_address_balance(
    value: &serde_json::Value,
) -> Result<AddressBalance, ParseError> {
    Ok(AddressBalance {
        balance: Zatoshis::new(as_u64(field(value, "balance")?)?)
            .map_err(|e| ParseError::Amount(e.to_string()))?,
        received: match opt_field(value, "received") {
            Some(v) => Zatoshis::new(as_u64(v)?).map_err(|e| ParseError::Amount(e.to_string()))?,
            None => Zatoshis::ZERO,
        },
    })
}

/// Parse a `getaddressdeltas` response.
pub(crate) fn parse_address_deltas(
    value: &serde_json::Value,
) -> Result<Vec<AddressDelta>, ParseError> {
    as_array(value)?
        .iter()
        .map(|d| {
            Ok(AddressDelta {
                satoshis: SignedZatoshis::new(as_i64(field(d, "satoshis")?)?),
                txid: as_txid(field(d, "txid")?)?,
                index: as_u32(field(d, "index")?)?,
                height: as_height(field(d, "height")?)?,
                address: TransparentAddress::new(as_str(field(d, "address")?)?.to_owned()),
                // zcashd emits `blockindex`; a validator that does not is
                // reported as not knowing it rather than as position zero.
                block_index: match opt_field(d, "blockindex") {
                    Some(v) => Some(as_u32(v)?),
                    None => None,
                },
            })
        })
        .collect()
}

/// Parse a `getaddressutxos` response.
pub(crate) fn parse_address_utxos(value: &serde_json::Value) -> Result<Vec<Utxo>, ParseError> {
    as_array(value)?
        .iter()
        .map(|u| {
            Ok(Utxo {
                address: TransparentAddress::new(as_str(field(u, "address")?)?.to_owned()),
                txid: as_txid(field(u, "txid")?)?,
                output_index: as_u32(field(u, "outputIndex")?)?,
                script: Script::new(
                    hex::decode(as_str(field(u, "script")?)?)
                        .map_err(|e| ParseError::Hex(e.to_string()))?,
                ),
                satoshis: Zatoshis::new(as_u64(field(u, "satoshis")?)?)
                    .map_err(|e| ParseError::Amount(e.to_string()))?,
                height: as_height(field(u, "height")?)?,
            })
        })
        .collect()
}

/// Parse a list of hex txids, as returned by `getrawmempool` and
/// `getaddresstxids`.
pub(crate) fn parse_txids(value: &serde_json::Value) -> Result<Vec<TransactionId>, ParseError> {
    as_array(value)?.iter().map(as_txid).collect()
}

/// Maximum number of entries accepted from one mempool listing, verbose or txid.
///
/// A ZIP-401-bounded validator cannot hold anything close to this — the cost
/// floor of 10,000 bytes per transaction caps an 80 MB mempool at roughly 8,000
/// entries — so this only ever trips on a validator that is compromised,
/// misconfigured, or impersonated.
///
/// It is a belt on top of `zaino_rpc::MAX_RESPONSE_BYTES`, which bounds the
/// response *bytes* but alone would still admit several hundred thousand txids,
/// each of which a consumer would then turn into a raw-transaction fetch.
pub(crate) const MAX_MEMPOOL_LISTING_ENTRIES: usize = 1_000_000;

/// Reject an over-cap mempool listing on its declared entry count, before any
/// entry is decoded.
///
/// Checking the count rather than the decoded set is the point: it bounds the
/// parse's peak allocation and, upstream, stops a pathological listing from
/// driving a million raw-transaction fetches.
fn enforce_listing_cap(kind: &'static str, len: usize) -> Result<(), ParseError> {
    if len > MAX_MEMPOOL_LISTING_ENTRIES {
        return Err(ParseError::ListingTooLarge {
            kind,
            len,
            max: MAX_MEMPOOL_LISTING_ENTRIES,
        });
    }
    Ok(())
}

/// Parse a `getrawmempool` response, under the mempool listing cap.
///
/// Separate from [`parse_txids`] because that also serves `getaddresstxids`,
/// where this bound has no meaning.
pub(crate) fn parse_mempool_txids(
    value: &serde_json::Value,
) -> Result<Vec<TransactionId>, ParseError> {
    let entries = as_array(value)?;
    enforce_listing_cap("txid", entries.len())?;
    entries.iter().map(as_txid).collect()
}

/// Parse a `getrawmempool verbose` response: a map of txid to `{ height, time }`.
///
/// Zebra reports far more per entry (fee, size, descendant stats); everything
/// beyond the entry height and time is ignored, because nothing Zaino serves is
/// derived from it and parsing a field commits us to its shape.
pub(crate) fn parse_mempool_metadata(
    value: &serde_json::Value,
) -> Result<Vec<MempoolTxMeta>, ParseError> {
    let entries = value
        .as_object()
        .ok_or_else(|| ParseError::unexpected("object", value))?;
    enforce_listing_cap("verbose", entries.len())?;

    entries
        .iter()
        .map(|(txid_hex, meta)| {
            Ok(MempoolTxMeta {
                txid: as_txid(&serde_json::Value::String(txid_hex.clone()))?,
                entry_height: as_height(field(meta, "height")?)?,
                // Absent rather than an error: the entry height is what Zaino
                // acts on, and a validator that omits the timestamp is still
                // giving a usable answer.
                entry_time: opt_field(meta, "time").map(as_i64).transpose()?,
            })
        })
        .collect()
}

/// Parse a `getrawtransaction(txid, 0)` response: a bare hex string.
pub(crate) fn parse_raw_transaction(value: &serde_json::Value) -> Result<Vec<u8>, ParseError> {
    hex::decode(as_str(value)?).map_err(|e| ParseError::Hex(e.to_string()))
}

/// Parse a `z_getsubtreesbyindex` response.
pub(crate) fn parse_subtree_roots(
    value: &serde_json::Value,
) -> Result<Vec<SubtreeRoot>, ParseError> {
    parse_optional_list(value, "subtrees", |s| {
        Ok(SubtreeRoot {
            root: as_tree_root(field(s, "root")?)?,
            end_height: as_height(field(s, "end_height")?)?,
        })
    })
}

/// Derive the per-pool roots and sizes from a `z_gettreestate` response.
///
/// # Why this deserialises a tree
///
/// `z_gettreestate` does not report roots or sizes directly. Zebra emits
/// `finalRoot` as `null` — its own type documents the field as unused — and no
/// `finalSize` field exists in the response at all. The only thing carried is
/// `finalState`: the serialised note commitment tree.
///
/// So the root and the size are *computed* here by deserialising that tree,
/// rather than read off the response. Reading the nominal fields would report
/// every pool as inactive against every Zebra node.
///
/// A pool with no `finalState` is treated as an empty tree rather than an
/// absent one: the pool exists at this height, it simply has no commitments
/// yet, and an empty tree has a well-defined root.
fn parse_tree_roots_inner(value: &serde_json::Value) -> Result<TreeRoots, ParseError> {
    Ok(TreeRoots {
        sapling: sapling_pool_root(opt_field(value, "sapling"))?,
        orchard: orchard_shaped_pool_root(opt_field(value, "orchard"))?,
        ironwood: orchard_shaped_pool_root(opt_field(value, "ironwood"))?,
    })
}

/// Public entry point, kept under the original name used by the adapter.
pub(crate) fn parse_tree_roots(value: &serde_json::Value) -> Result<TreeRoots, ParseError> {
    parse_tree_roots_inner(value)
}

/// The serialised tree for one pool, if the response carries that pool at all.
fn pool_final_state(pool: Option<&serde_json::Value>) -> Result<Option<Vec<u8>>, ParseError> {
    let Some(pool) = pool else { return Ok(None) };
    let Some(commitments) = opt_field(pool, "commitments") else {
        return Ok(None);
    };
    match opt_field(commitments, "finalState") {
        Some(state) => Ok(Some(
            hex::decode(as_str(state)?).map_err(|e| ParseError::Hex(e.to_string()))?,
        )),
        // The pool is present but empty — a well-defined state, not an absent
        // pool, so it still yields a root below.
        None => Ok(Some(Vec::new())),
    }
}

fn sapling_pool_root(pool: Option<&serde_json::Value>) -> Result<Option<TreeRootInfo>, ParseError> {
    let Some(bytes) = pool_final_state(pool)? else {
        return Ok(None);
    };
    let tree = read_tree::<sapling_crypto::Node>(&bytes)?;
    Ok(Some(TreeRootInfo {
        root: TreeRoot::new(tree.root().to_bytes()),
        size: tree.size() as u64,
    }))
}

/// Orchard and Ironwood share a node type and a root representation, so they
/// share this reader — the pools differ only in which field they came from.
fn orchard_shaped_pool_root(
    pool: Option<&serde_json::Value>,
) -> Result<Option<TreeRootInfo>, ParseError> {
    let Some(bytes) = pool_final_state(pool)? else {
        return Ok(None);
    };
    let tree = read_tree::<zebra_chain::orchard::tree::Node>(&bytes)?;
    Ok(Some(TreeRootInfo {
        root: TreeRoot::new(tree.root().to_repr()),
        size: tree.size() as u64,
    }))
}

/// Deserialise a note commitment tree, treating empty bytes as an empty tree.
fn read_tree<N>(bytes: &[u8]) -> Result<CommitmentTree<N, 32>, ParseError>
where
    N: incrementalmerkletree::Hashable + Clone + zcash_primitives::merkle_tree::HashSer,
{
    if bytes.is_empty() {
        return Ok(CommitmentTree::empty());
    }
    zcash_primitives::merkle_tree::read_commitment_tree(bytes)
        .map_err(|e| ParseError::Deserialize(format!("note commitment tree: {e}")))
}

/// Parse a `getblockchaininfo` response.
///
/// Zaino adopts [`BlockchainInfo::upgrades`] as its activation schedule, so a
/// malformed upgrade entry fails the whole parse rather than being skipped: a
/// silently short schedule would put Zaino on different consensus rules from
/// its validator.
pub(crate) fn parse_blockchain_info(
    value: &serde_json::Value,
) -> Result<BlockchainInfo, ParseError> {
    let consensus = field(value, "consensus")?;
    Ok(BlockchainInfo {
        chain: as_str(field(value, "chain")?)?.to_owned(),
        blocks: as_height(field(value, "blocks")?)?,
        headers: as_height(field(value, "headers")?)?,
        estimated_height: as_height(field(value, "estimatedheight")?)?,
        best_block_hash: parse_block_hash(field(value, "bestblockhash")?)?,
        difficulty: as_f64(field(value, "difficulty")?)?,
        verification_progress: as_f64(field(value, "verificationprogress")?)?,
        chain_work: parse_reported_chain_work(field(value, "chainwork")?)?,
        pruned: opt_field(value, "pruned")
            .map(as_bool)
            .transpose()?
            .unwrap_or(false),
        size_on_disk: opt_field(value, "size_on_disk")
            .map(as_u64)
            .transpose()?
            .unwrap_or(0),
        commitments: opt_field(value, "commitments")
            .map(as_u64)
            .transpose()?
            .unwrap_or(0),
        chain_supply: parse_value_pool(field(value, "chainSupply")?)?,
        value_pools: parse_optional_list(value, "valuePools", parse_value_pool)?,
        upgrades: parse_upgrades(opt_field(value, "upgrades"))?,
        consensus: ConsensusBranchIds {
            chain_tip: parse_branch_id(field(consensus, "chaintip")?)?,
            next_block: parse_branch_id(field(consensus, "nextblock")?)?,
        },
    })
}

/// Parse cumulative chainwork, which crosses the wire as a hex string.
///
/// Accepts fewer than 32 bytes and left-pads: the value is a big-endian integer
/// and validators trim leading zeroes, so an early-chain response is genuinely
/// short rather than malformed. Anything longer than 32 bytes is out of range
/// for the protocol and is rejected.
/// Chainwork as reported in `getblockchaininfo`, where the two validators
/// disagree on both the encoding and whether they track it at all.
///
/// zcashd sends a hex string. Zebra types the field as a 64-bit integer, so it
/// arrives as a JSON number, and hardcodes it to zero because it does not store
/// cumulative work per height. Zero is not a possible amount of work for a real
/// chain, so it is read as "not reported" rather than as a value a consumer
/// could compare.
fn parse_reported_chain_work(value: &serde_json::Value) -> Result<Option<ChainWork>, ParseError> {
    if let Some(number) = value.as_u64() {
        if number == 0 {
            return Ok(None);
        }
        let mut be = [0u8; 32];
        be[24..].copy_from_slice(&number.to_be_bytes());
        return Ok(Some(ChainWork::new(be)));
    }

    let work = as_chain_work_natural(value)?;
    Ok((work != ChainWork::new([0u8; 32])).then_some(work))
}

/// Cumulative chainwork as a hex string. Natural order, and left-padded rather
/// than fixed width: it is a big-endian integer, so validators trim leading
/// zeroes and an early-chain response is genuinely short rather than malformed.
fn as_chain_work_natural(value: &serde_json::Value) -> Result<ChainWork, ParseError> {
    let s = as_str(value)?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    let padded = format!("{s:0>64}");
    let bytes = hex::decode(&padded).map_err(|e| ParseError::Hex(e.to_string()))?;
    let be: [u8; 32] = bytes
        .try_into()
        .map_err(|b: Vec<u8>| ParseError::WrongLength {
            expected: 32,
            got: b.len(),
        })?;
    Ok(ChainWork::new(be))
}

fn parse_branch_id(value: &serde_json::Value) -> Result<ConsensusBranchId, ParseError> {
    let s = as_str(value)?;
    u32::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
        .map(ConsensusBranchId::new)
        .map_err(|e| ParseError::Hex(format!("consensus branch id `{s}`: {e}")))
}

fn parse_value_pool(value: &serde_json::Value) -> Result<ValuePoolBalance, ParseError> {
    Ok(ValuePoolBalance {
        id: opt_field(value, "id")
            .map(|v| as_str(v).map(str::to_owned))
            .transpose()?
            .unwrap_or_default(),
        chain_value: zatoshis_field(value, "chainValueZat", "chainValue")?,
        monitored: opt_field(value, "monitored")
            .map(as_bool)
            .transpose()?
            .unwrap_or(true),
        value_delta: opt_field(value, "valueDeltaZat")
            .map(as_i64)
            .transpose()?
            .map(SignedZatoshis::new),
    })
}

/// Parse the network upgrade schedule, keyed on disk by consensus branch id.
///
/// The branch id is the map key rather than a field, so it is read from there
/// and carried into each entry — it is the upgrade's protocol identity, whereas
/// the name is only a label.
fn parse_upgrades(
    value: Option<&serde_json::Value>,
) -> Result<Vec<NetworkUpgradeInfo>, ParseError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let map = value
        .as_object()
        .ok_or_else(|| ParseError::unexpected("object", value))?;

    map.iter()
        .map(|(branch_id, info)| {
            Ok(NetworkUpgradeInfo {
                branch_id: parse_branch_id(&serde_json::Value::String(branch_id.clone()))?,
                name: opt_field(info, "name")
                    .map(|v| as_str(v).map(str::to_owned))
                    .transpose()?
                    .unwrap_or_default(),
                activation_height: as_height(field(info, "activationheight")?)?,
                status: match as_str(field(info, "status")?)? {
                    "active" => NetworkUpgradeStatus::Active,
                    "pending" => NetworkUpgradeStatus::Pending,
                    "disabled" => NetworkUpgradeStatus::Disabled,
                    other => {
                        return Err(ParseError::Amount(format!(
                            "unknown network upgrade status `{other}`"
                        )))
                    }
                },
            })
        })
        .collect()
}

/// Parse the chain-state fields of a verbose `getblock` response.
///
/// Reads only what cannot be derived from the block's own bytes — see
/// [`BlockVerbose`]. Everything else in the response is deliberately ignored
/// here rather than duplicated into a second source of the same fact.
pub(crate) fn parse_block_verbose(value: &serde_json::Value) -> Result<BlockVerbose, ParseError> {
    let trees = opt_field(value, "trees");
    Ok(BlockVerbose {
        confirmations: as_i64(field(value, "confirmations")?)?,
        difficulty: as_f64(field(value, "difficulty")?)?,
        chainwork: opt_field(value, "chainwork")
            .map(parse_reported_chain_work)
            .transpose()?
            .flatten(),
        chain_supply: opt_field(value, "chainSupply")
            .map(parse_value_pool)
            .transpose()?,
        value_pools: parse_optional_list(value, "valuePools", parse_value_pool)?,
        tree_sizes: BlockTreeSizes {
            sapling: pool_tree_size(trees, "sapling")?,
            orchard: pool_tree_size(trees, "orchard")?,
            ironwood: pool_tree_size(trees, "ironwood")?,
        },
        next_block_hash: opt_field(value, "nextblockhash")
            .map(parse_block_hash)
            .transpose()?,
    })
}

/// One pool's cumulative tree size from the `trees` object.
///
/// Absent means the pool is not active at this block, which is a size of zero
/// rather than unknown — a pool with no activation has committed no notes.
fn pool_tree_size(trees: Option<&serde_json::Value>, pool: &str) -> Result<u64, ParseError> {
    let Some(size) = trees
        .and_then(|t| t.get(pool))
        .and_then(|p| opt_field(p, "size"))
    else {
        return Ok(0);
    };
    as_u64(size)
}

/// Parse a `getblockdeltas` response.
pub(crate) fn parse_block_deltas(value: &serde_json::Value) -> Result<BlockDeltas, ParseError> {
    Ok(BlockDeltas {
        hash: parse_block_hash(field(value, "hash")?)?,
        confirmations: as_i64(field(value, "confirmations")?)?,
        size: as_u64(field(value, "size")?)?,
        height: as_height(field(value, "height")?)?,
        version: as_u32(field(value, "version")?)?,
        merkle_root: as_merkle_root(field(value, "merkleroot")?)?,
        time: as_u32(field(value, "time")?)?,
        median_time: as_u32(field(value, "mediantime")?)?,
        nonce: as_nonce(field(value, "nonce")?)?,
        bits: parse_compact_difficulty(field(value, "bits")?)?,
        difficulty: as_f64(field(value, "difficulty")?)?,
        previous_block_hash: opt_field(value, "previousblockhash")
            .map(parse_block_hash)
            .transpose()?,
        next_block_hash: opt_field(value, "nextblockhash")
            .map(parse_block_hash)
            .transpose()?,
        deltas: parse_optional_list(value, "deltas", |d| {
            Ok(BlockDelta {
                txid: as_txid(field(d, "txid")?)?,
                index: as_u32(field(d, "index")?)?,
                inputs: parse_optional_list(d, "inputs", |i| {
                    Ok(InputDelta {
                        address: TransparentAddress::new(as_str(field(i, "address")?)?.to_owned()),
                        satoshis: SignedZatoshis::new(as_i64(field(i, "satoshis")?)?),
                        index: as_u32(field(i, "index")?)?,
                        prev_txid: as_txid(field(i, "prevtxid")?)?,
                        prev_output: as_u32(field(i, "prevout")?)?,
                    })
                })?,
                outputs: parse_optional_list(d, "outputs", |o| {
                    Ok(OutputDelta {
                        address: TransparentAddress::new(as_str(field(o, "address")?)?.to_owned()),
                        satoshis: Zatoshis::new(as_u64(field(o, "satoshis")?)?)
                            .map_err(|e| ParseError::Amount(e.to_string()))?,
                        index: as_u32(field(o, "index")?)?,
                    })
                })?,
            })
        })?,
    })
}

/// Parse a verbose `getrawtransaction` response into raw bytes plus location.
///
/// # Location
///
/// The `height` field carries all three placements, and the distinction
/// matters: reporting a side-chain transaction as unmined would tell a caller
/// it is still pending when it is in fact on an abandoned branch.
///
/// - absent — the transaction is in the mempool, not mined anywhere;
/// - `-1` — mined, but in a side-chain block;
/// - `>= 0` — mined at that height in the best chain.
///
/// Any other negative value is rejected rather than folded into
/// [`TransactionLocation::NonBestChain`]: `-1` is the defined sentinel, and a
/// validator sending something else is not making a statement this interface
/// defines.
pub(crate) fn parse_transaction(
    value: &serde_json::Value,
) -> Result<TransactionResponse, ParseError> {
    let bytes =
        hex::decode(as_str(field(value, "hex")?)?).map_err(|e| ParseError::Hex(e.to_string()))?;

    let location = match opt_field(value, "height").map(as_i64).transpose()? {
        None => TransactionLocation::Mempool,
        Some(-1) => TransactionLocation::NonBestChain,
        Some(height) if height >= 0 => {
            let height = u32::try_from(height).map_err(|_| ParseError::Overflow(height as u64))?;
            TransactionLocation::BestChain(
                Height::try_from(height).map_err(|e| ParseError::Height(e.to_string()))?,
            )
        }
        Some(other) => {
            return Err(ParseError::Height(format!(
                "transaction height {other} is neither a best-chain height nor the \
                 side-chain sentinel -1"
            )))
        }
    };

    Ok(TransactionResponse { bytes, location })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A value whose reversal is unmistakable: it reads one way forwards and
    /// another backwards, so a mirrored decode cannot pass by coincidence.
    const ASYMMETRIC_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddee01";

    fn asymmetric_bytes() -> [u8; 32] {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(ASYMMETRIC_HEX, &mut bytes).expect("valid fixture");
        bytes
    }

    fn reversed_bytes() -> [u8; 32] {
        let mut bytes = asymmetric_bytes();
        bytes.reverse();
        bytes
    }

    /// Byte order is a property of each domain type, and getting it wrong
    /// mirrors a value silently rather than failing. These assertions are the
    /// only thing standing between a mistyped constructor and a wrong hash, so
    /// they pin every 32-byte type this module decodes.
    ///
    /// The expected directions are taken from zebra's own serde: `block::Hash`
    /// and `merkle::Root` implement `FromHex` by reversing, while the plain
    /// `[u8; 32]` header fields do not.
    #[test]
    fn hashes_and_merkle_roots_are_reversed_on_the_wire() {
        let value = json!(ASYMMETRIC_HEX);

        assert_eq!(
            <[u8; 32]>::from(parse_block_hash(&value).expect("block hash")),
            reversed_bytes()
        );
        assert_eq!(
            <[u8; 32]>::from(as_txid(&value).expect("txid")),
            reversed_bytes()
        );
        assert_eq!(
            as_merkle_root(&value).expect("merkle root"),
            MerkleRoot::from(reversed_bytes())
        );
    }

    #[test]
    fn commitments_roots_and_nonces_keep_their_natural_order() {
        let value = json!(ASYMMETRIC_HEX);

        assert_eq!(
            as_block_commitments(&value).expect("commitments"),
            BlockCommitments::from(asymmetric_bytes())
        );
        assert_eq!(
            as_tree_root(&value).expect("tree root"),
            TreeRoot::new(asymmetric_bytes())
        );
        assert_eq!(as_nonce(&value).expect("nonce"), asymmetric_bytes());
    }

    /// Chainwork is a big-endian integer, so validators trim leading zeroes.
    /// A short value must left-pad to the same number, not be rejected or
    /// right-aligned into a different one.
    #[test]
    fn chainwork_left_pads_a_trimmed_value() {
        let trimmed = as_chain_work_natural(&json!("ff")).expect("short chainwork");

        let mut expected = [0u8; 32];
        expected[31] = 0xff;
        assert_eq!(trimmed, ChainWork::new(expected));
    }

    /// The health sentinels differ per method, and both must read as "healthy"
    /// rather than reaching a consumer as a message it would display.
    #[test]
    fn health_sentinels_normalise_to_none() {
        assert_eq!(
            parse_health_sentinel(Some(&json!("no errors")), "no errors").unwrap(),
            None
        );
        assert_eq!(parse_health_sentinel(Some(&json!("")), "").unwrap(), None);
        assert_eq!(parse_health_sentinel(None, "no errors").unwrap(), None);
        assert_eq!(
            parse_health_sentinel(Some(&json!("disk full")), "no errors").unwrap(),
            Some("disk full".to_owned())
        );
    }

    /// The exact zatoshi field wins over its ZEC twin, so no amount is routed
    /// through a float when the validator already sent an integer.
    #[test]
    fn amounts_prefer_the_exact_integer_field() {
        let both = json!({ "value": 1.0, "valueZat": 99_999_999u64 });
        assert_eq!(
            zatoshis_field(&both, "valueZat", "value").expect("integer field"),
            Zatoshis::new(99_999_999).expect("valid")
        );

        let zec_only = json!({ "value": 1.5 });
        assert_eq!(
            zatoshis_field(&zec_only, "valueZat", "value").expect("zec fallback"),
            Zatoshis::new(150_000_000).expect("valid")
        );
    }

    /// Zebra defines `height` as carrying all three placements. Reporting a
    /// side-chain transaction as unmined would tell a caller it is still
    /// pending when it is actually on an abandoned branch, so each case is
    /// pinned.
    #[test]
    fn transaction_location_distinguishes_all_three_placements() {
        let mined = json!({ "hex": "00", "height": 12345 });
        assert_eq!(
            parse_transaction(&mined).expect("mined").location,
            TransactionLocation::BestChain(Height::try_from(12345).expect("valid"))
        );

        let side_chain = json!({ "hex": "00", "height": -1 });
        assert_eq!(
            parse_transaction(&side_chain).expect("side chain").location,
            TransactionLocation::NonBestChain
        );

        let mempool = json!({ "hex": "00" });
        assert_eq!(
            parse_transaction(&mempool).expect("mempool").location,
            TransactionLocation::Mempool
        );
    }

    /// Only `-1` means side chain. Another negative value is not a statement
    /// this interface defines, so it must not be silently accepted as one.
    #[test]
    fn transaction_rejects_an_undefined_negative_height() {
        let bogus = json!({ "hex": "00", "height": -7 });

        assert!(parse_transaction(&bogus).is_err());
    }

    /// `z_gettreestate` reports neither a root nor a size — zebra sends
    /// `finalRoot: null` and there is no `finalSize` field — so both are
    /// derived from the serialised tree in `finalState`. Reading the nominal
    /// fields instead would report every pool as inactive against every Zebra
    /// node, which is what this pins against.
    #[test]
    fn tree_roots_are_derived_from_final_state_not_read_from_fields() {
        // A pool present but with no commitments yet: an empty tree, which has
        // a well-defined root, not an absent pool.
        let empty_pools = json!({
            "sapling": { "commitments": { "finalState": "" } },
            "orchard": { "commitments": { "finalState": "" } },
        });

        let roots = parse_tree_roots(&empty_pools).expect("empty trees are valid");

        let sapling = roots.sapling.expect("sapling pool present");
        assert_eq!(sapling.size, 0, "an empty tree holds no commitments");
        let orchard = roots.orchard.expect("orchard pool present");
        assert_eq!(orchard.size, 0);
        assert!(
            roots.ironwood.is_none(),
            "a pool absent from the response stays absent"
        );
    }

    /// A response shaped the way the nominal fields suggest — carrying
    /// `finalRoot` but no `finalState` — must not be silently read as a root.
    #[test]
    fn a_pool_without_final_state_is_still_an_empty_tree() {
        let root_only = json!({
            "sapling": { "commitments": { "finalRoot": "ab".repeat(32) } },
        });

        let roots = parse_tree_roots(&root_only).expect("parses");
        let sapling = roots.sapling.expect("pool present");

        assert_eq!(
            sapling.size, 0,
            "size comes from the tree, and there is no tree here"
        );
    }

    /// Every pool the validator reports must reach the domain, keyed by its own
    /// `id`. The list is positional on the wire, so a dropped or misordered
    /// entry silently attributes value to the wrong pool.
    ///
    /// This covers what `zaino-fetch`'s `parses_five_value_pools` covered before
    /// its crate was deleted; the parse it exercised now lives here.
    #[test]
    fn every_reported_value_pool_reaches_the_domain() {
        let info = parse_blockchain_info(&serde_json::json!({
            "chain": "regtest",
            "blocks": 100,
            "headers": 100,
            "estimatedheight": 100,
            "bestblockhash": "00".repeat(32),
            "difficulty": 1.0,
            "verificationprogress": 1.0,
            "chainwork": "00",
            "chainSupply": { "chainValueZat": 1_000u64 },
            "valuePools": [
                { "id": "transparent", "chainValueZat": 1u64 },
                { "id": "sprout", "chainValueZat": 2u64 },
                { "id": "sapling", "chainValueZat": 3u64 },
                { "id": "orchard", "chainValueZat": 4u64 },
                { "id": "ironwood", "chainValueZat": 5u64 },
            ],
            "consensus": { "chaintip": "00000000", "nextblock": "00000000" },
        }))
        .expect("a well-formed getblockchaininfo parses");

        assert_eq!(
            info.value_pools
                .iter()
                .map(|pool| (pool.id.as_str(), u64::from(pool.chain_value)))
                .collect::<Vec<_>>(),
            vec![
                ("transparent", 1),
                ("sprout", 2),
                ("sapling", 3),
                ("orchard", 4),
                ("ironwood", 5),
            ]
        );
        assert_eq!(u64::from(info.chain_supply.chain_value), 1_000);
    }

    /// The wire reports each pool balance twice — an exact `chainValueZat` and a
    /// ZEC `chainValue` float. A large mainnet balance has a `chainValue` that
    /// does not round-trip to a whole zatoshi: real sapling `529544.04149098`
    /// gives `529544.04149098 * 1e8 = 52954404149097.99`, off by a fraction of a
    /// zatoshi. The domain must read the exact integer and never the float.
    ///
    /// Regression gate for the mainnet-boot crash: `adopt_network` bypassed this
    /// parser and deserialized into zebra's `Zec`-typed response, whose
    /// `try_from = "f64"` rejected exactly this value with "floating point had
    /// fractional zatoshis". Reading `chainValueZat` here is what makes the
    /// domain immune, so if this ever flips to the float, boot breaks again.
    #[test]
    fn a_value_pool_reads_the_exact_zatoshi_over_a_lossy_zec_float() {
        // A real mainnet sapling balance, reported both ways. The ZEC float does
        // not round-trip: `SAPLING_ZEC * 1e8 = 52954404149097.99`, a fractional
        // zatoshi. Reading SAPLING_ZAT is what keeps the domain immune.
        const SAPLING_ZAT: u64 = 52_954_404_149_098;
        const SAPLING_ZEC: f64 = 529_544.04149098;

        let info = parse_blockchain_info(&serde_json::json!({
            "chain": "main",
            "blocks": 3_451_543,
            "headers": 3_451_543,
            "estimatedheight": 3_451_544,
            "bestblockhash": "00".repeat(32),
            "difficulty": 1.0,
            "verificationprogress": 1.0,
            "chainwork": "00",
            "chainSupply": { "chainValue": 16_882_668.9155448, "chainValueZat": 1_688_266_891_554_480u64 },
            "valuePools": [
                { "id": "sapling", "chainValue": SAPLING_ZEC, "chainValueZat": SAPLING_ZAT },
            ],
            "consensus": { "chaintip": "00000000", "nextblock": "00000000" },
        }))
        .expect("a getblockchaininfo with lossy value-pool floats must still parse");

        let sapling = info
            .value_pools
            .iter()
            .find(|pool| pool.id == "sapling")
            .expect("sapling pool present");
        assert_eq!(
            u64::from(sapling.chain_value),
            SAPLING_ZAT,
            "must read the exact chainValueZat, not the lossy chainValue float"
        );
    }

    /// A validator that reports no pools at all is not an error: the field is
    /// optional, and an empty list says exactly that.
    #[test]
    fn absent_value_pools_parse_as_an_empty_list() {
        let info = parse_blockchain_info(&serde_json::json!({
            "chain": "regtest",
            "blocks": 0,
            "headers": 0,
            "estimatedheight": 0,
            "bestblockhash": "00".repeat(32),
            "difficulty": 1.0,
            "verificationprogress": 1.0,
            "chainwork": "00",
            "chainSupply": { "chainValueZat": 0u64 },
            "consensus": { "chaintip": "00000000", "nextblock": "00000000" },
        }))
        .expect("a getblockchaininfo without pools parses");

        assert!(info.value_pools.is_empty());
    }

    /// The verbose listing carries far more per entry than Zaino reads. Only
    /// `height` and `time` are taken, and an entry missing `time` is still a
    /// usable answer — the entry height is the field Zaino acts on.
    #[test]
    fn verbose_mempool_takes_only_the_entry_height_and_time() {
        let value = json!({
            ASYMMETRIC_HEX: {
                "size": 1_234,
                "fee": 1_000,
                "time": 1_700_000_000i64,
                "height": 2_500_000,
                "descendantcount": 1,
                "depends": [],
            },
        });

        let entries = parse_mempool_metadata(&value).expect("verbose mempool parses");

        assert_eq!(entries.len(), 1);
        assert_eq!(u32::from(entries[0].entry_height), 2_500_000);
        assert_eq!(entries[0].entry_time, Some(1_700_000_000));
        assert_eq!(
            <[u8; 32]>::from(entries[0].txid),
            reversed_bytes(),
            "txids are reversed on the wire, keys included"
        );
    }

    /// An entry without `time` parses; one without `height` does not. The
    /// timestamp is informational, but the entry height is a protocol field
    /// Zaino stamps onto its mempool entries — inventing one would put a wrong
    /// consensus branch id on a served transaction.
    #[test]
    fn a_verbose_entry_needs_its_height_but_not_its_time() {
        let without_time = json!({ ASYMMETRIC_HEX: { "height": 2_500_000 } });
        let entries = parse_mempool_metadata(&without_time).expect("height alone parses");
        assert_eq!(entries[0].entry_time, None);

        let without_height = json!({ ASYMMETRIC_HEX: { "time": 1_700_000_000i64 } });
        assert!(matches!(
            parse_mempool_metadata(&without_height),
            Err(ParseError::MissingField("height"))
        ));
    }

    /// The cap is checked on the declared entry count, before any entry is
    /// decoded — that is what bounds the parse's peak allocation and, upstream,
    /// stops a pathological listing from driving a million raw-transaction
    /// fetches. At the cap is accepted; one over is refused.
    #[test]
    fn an_oversized_mempool_listing_is_refused_on_its_count() {
        assert!(enforce_listing_cap("txid", MAX_MEMPOOL_LISTING_ENTRIES).is_ok());

        for kind in ["txid", "verbose"] {
            assert!(
                matches!(
                    enforce_listing_cap(kind, MAX_MEMPOOL_LISTING_ENTRIES + 1),
                    Err(ParseError::ListingTooLarge { .. })
                ),
                "the {kind} listing must be capped"
            );
        }
    }

    /// `getrawtransaction` at verbosity 0 answers with a bare hex string, not
    /// an object — a different shape from the verbosity-1 response
    /// [`parse_transaction`] reads.
    #[test]
    fn a_raw_transaction_is_a_bare_hex_string() {
        assert_eq!(
            parse_raw_transaction(&json!("deadbeef")).expect("hex parses"),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert!(matches!(
            parse_raw_transaction(&json!({ "hex": "deadbeef" })),
            Err(ParseError::UnexpectedType { .. })
        ));
    }
}
