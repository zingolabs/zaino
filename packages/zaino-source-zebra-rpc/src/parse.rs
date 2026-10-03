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
        FundingStream, InputDelta, LocalAddress, LockboxStream, MiningInfo, NetworkEntry,
        NetworkInfo, NodeInfo, OutputDelta, PeerInfo, ScriptPubKey, SpentInfo, TxOut,
    },
    AbsoluteChainWork, AddressBalance, AddressDelta, BlockCommitments, BlockHash, BlockTreeSizes,
    BlockVerbose, BlockchainInfo, CompactDifficulty, ConsensusBranchId, ConsensusBranchIds, Height,
    MerkleRoot, NetworkUpgradeInfo, NetworkUpgradeStatus, Script, SignedZatoshis, SubtreeRoot,
    TransactionId, TransactionLocation, TransparentAddress, TreeRoot, TreeRootInfo, TreeRoots,
    TreeSize, TreeSizeOutOfRange, Treestate, Utxo, ValuePoolBalance, Zatoshis, ZatoshisFlowSum,
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

    /// Reported chainwork does not fit the domain's recorded width.
    #[error("chainwork: {0}")]
    AbsoluteChainWork(zaino_primitives::types::ChainWorkOverWidth),

    /// Reported nBits is not a valid compact difficulty encoding.
    #[error("nBits: {0}")]
    CompactDifficulty(zaino_primitives::types::CompactDifficultyError),

    /// A reported commitment tree size does not fit a [`TreeSize`].
    #[error("tree size: {0}")]
    TreeSize(#[from] TreeSizeOutOfRange),

    /// Height validation failed.
    #[error("invalid height: {0}")]
    Height(String),

    /// A required field was absent from the response.
    #[error("missing field `{0}`")]
    MissingField(&'static str),

    /// A monetary amount was invalid or out of range.
    #[error("invalid amount: {0}")]
    Amount(String),

    /// A ZEC-denominated amount (e.g. a mempool entry's `fee`) could not be
    /// converted to zatoshis.
    #[error(transparent)]
    ZecAmount(#[from] ZecAmountError),

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

/// Why a ZEC-denominated decimal amount could not be read as zatoshis.
///
/// The validator reports amounts such as a mempool entry's `fee` as a ZEC JSON
/// number. The conversion to zatoshis is done on that number's *decimal text*,
/// never through an `f64` multiply, so a ZEC value with up to eight fractional
/// digits maps to its exact zatoshi integer. These are its refusals; the
/// malformed-text case keeps the underlying parse failure in its source chain.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub(crate) enum ZecAmountError {
    /// The text was not a decimal number (bad digit, empty, or an exponent that
    /// did not parse).
    #[error("malformed ZEC amount {text:?}")]
    Malformed {
        /// The offending text, as the validator sent it.
        text: String,
        /// The integer-parse failure underneath.
        #[source]
        source: std::num::ParseIntError,
    },
    /// A negative amount, where the field is defined as non-negative.
    #[error("negative ZEC amount {text:?}")]
    Negative {
        /// The offending text.
        text: String,
    },
    /// The amount carries detail below one zatoshi (more than eight fractional
    /// digits, with a non-zero digit past the eighth).
    #[error("ZEC amount {text:?} is finer than one zatoshi")]
    TooPrecise {
        /// The offending text.
        text: String,
    },
    /// The amount does not fit the zatoshi range (above the money supply, or an
    /// exponent so large the scaled value overflows).
    #[error("ZEC amount {text:?} is out of range")]
    OutOfRange {
        /// The offending text.
        text: String,
    },
}

/// Convert a ZEC-denominated decimal amount, as text, to zatoshis — exactly.
///
/// The value may carry an exponent (`1e-8`) or a decimal point (`0.00001`), the
/// two forms serde renders a JSON number in. It is scaled by `10^8` using
/// integer arithmetic on the digits, so the result is the exact zatoshi count
/// and no `f64` is multiplied. An amount finer than one zatoshi, negative, or
/// beyond the supply is refused rather than rounded.
fn zec_text_to_zatoshis(text: &str) -> Result<Zatoshis, ZecAmountError> {
    let malformed = |source: std::num::ParseIntError| ZecAmountError::Malformed {
        text: text.to_owned(),
        source,
    };

    // Split off an exponent, if any.
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exp)) => (mantissa, exp.parse::<i32>().map_err(malformed)?),
        None => (text, 0),
    };

    // A fee is non-negative; a leading sign that is not `+` is a negative value.
    if mantissa.starts_with('-') {
        return Err(ZecAmountError::Negative {
            text: text.to_owned(),
        });
    }
    let mantissa = mantissa.strip_prefix('+').unwrap_or(mantissa);

    // Separate the integer and fractional digit runs around the point.
    let (int_digits, frac_digits) = match mantissa.split_once('.') {
        Some((int_digits, frac_digits)) => (int_digits, frac_digits),
        None => (mantissa, ""),
    };

    // All significant digits as one integer; `u128::from_str` rejects any
    // non-digit (including an internal sign or second point) and keeps its
    // failure as the source.
    let digits = format!("{int_digits}{frac_digits}");
    let value: u128 = digits.parse::<u128>().map_err(malformed)?;

    // Power of ten to reach zatoshis: eight for the ZEC→zatoshi scale, less the
    // fractional digits already shifted in, plus the exponent. Every step is
    // checked — a fractional run or an exponent large enough to overflow the
    // scale is an out-of-range amount, not a wrapped one.
    let out_of_range = || ZecAmountError::OutOfRange {
        text: text.to_owned(),
    };
    let frac_len = i32::try_from(frac_digits.len()).map_err(|_| out_of_range())?;
    let scale = 8i32
        .checked_sub(frac_len)
        .and_then(|partial| partial.checked_add(exponent))
        .ok_or_else(out_of_range)?;

    let zatoshis: u128 = if scale >= 0 {
        let factor = power_of_ten(scale, text)?;
        value.checked_mul(factor).ok_or_else(out_of_range)?
    } else {
        let negated = scale.checked_neg().ok_or_else(out_of_range)?;
        let divisor = power_of_ten(negated, text)?;
        if !value.is_multiple_of(divisor) {
            return Err(ZecAmountError::TooPrecise {
                text: text.to_owned(),
            });
        }
        value / divisor
    };

    let zatoshis = u64::try_from(zatoshis).map_err(|_| ZecAmountError::OutOfRange {
        text: text.to_owned(),
    })?;
    Zatoshis::new(zatoshis).map_err(|_| ZecAmountError::OutOfRange {
        text: text.to_owned(),
    })
}

/// `10^exp` as a `u128`, treating an overflow (an absurd exponent) as an
/// out-of-range amount rather than a panic.
fn power_of_ten(exp: i32, text: &str) -> Result<u128, ZecAmountError> {
    let exp = u32::try_from(exp).map_err(|_| ZecAmountError::OutOfRange {
        text: text.to_owned(),
    })?;
    10u128
        .checked_pow(exp)
        .ok_or_else(|| ZecAmountError::OutOfRange {
            text: text.to_owned(),
        })
}

/// Parse a ZEC-denominated JSON number field (e.g. a mempool entry's `fee`) into
/// zatoshis, from the number's decimal text.
fn parse_fee(value: &serde_json::Value) -> Result<Zatoshis, ParseError> {
    let number = value
        .as_number()
        .ok_or_else(|| ParseError::unexpected("number", value))?;
    Ok(zec_text_to_zatoshis(&number.to_string())?)
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
            .map(parse_reported_chain_work)
            .transpose()?
            .flatten(),
        previous_block_hash: opt_field(value, "previousblockhash")
            .map(parse_block_hash)
            .transpose()?,
        next_block_hash: opt_field(value, "nextblockhash")
            .map(parse_block_hash)
            .transpose()?,
    })
}

/// Parse the compact difficulty (`nBits`), which crosses the wire as hex.
///
/// The hex decode recovers the raw `u32`; the primitives door then applies the
/// encoding's acceptance set, so a malformed difficulty fails the parse here
/// rather than riding through the block shapes.
fn parse_compact_difficulty(value: &serde_json::Value) -> Result<CompactDifficulty, ParseError> {
    let s = as_str(value)?;
    let bits = u32::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
        .map_err(|e| ParseError::Hex(format!("nBits `{s}`: {e}")))?;
    CompactDifficulty::try_from_bits(bits).map_err(ParseError::CompactDifficulty)
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

/// Parse a `getnetworkinfo` response.
pub(crate) fn parse_network_info(value: &serde_json::Value) -> Result<NetworkInfo, ParseError> {
    Ok(NetworkInfo {
        version: as_u64(field(value, "version")?)?,
        subversion: as_str(field(value, "subversion")?)?.to_owned(),
        protocol_version: as_u32(field(value, "protocolversion")?)?,
        local_services: as_str(field(value, "localservices")?)?.to_owned(),
        time_offset: as_i64(field(value, "timeoffset")?)?,
        connections: as_u64(field(value, "connections")?)?,
        networks: parse_optional_list(value, "networks", parse_network_entry)?,
        relay_fee: zec_to_zatoshis(as_f64(field(value, "relayfee")?)?)?,
        local_addresses: parse_optional_list(value, "localaddresses", parse_local_address)?,
        // zebra/zcashd report a string; absent or null is "no warnings".
        warnings: opt_field(value, "warnings")
            .map(|v| as_str(v).map(str::to_owned))
            .transpose()?
            .unwrap_or_default(),
    })
}

/// Parse one `networks` entry of a `getnetworkinfo` response.
fn parse_network_entry(value: &serde_json::Value) -> Result<NetworkEntry, ParseError> {
    Ok(NetworkEntry {
        name: as_str(field(value, "name")?)?.to_owned(),
        limited: as_bool(field(value, "limited")?)?,
        reachable: as_bool(field(value, "reachable")?)?,
        proxy: opt_field(value, "proxy")
            .map(|v| as_str(v).map(str::to_owned))
            .transpose()?
            .unwrap_or_default(),
        proxy_randomize_credentials: opt_field(value, "proxy_randomize_credentials")
            .map(as_bool)
            .transpose()?
            .unwrap_or(false),
    })
}

/// Parse one `localaddresses` entry of a `getnetworkinfo` response.
fn parse_local_address(value: &serde_json::Value) -> Result<LocalAddress, ParseError> {
    let port = as_u64(field(value, "port")?)?;
    Ok(LocalAddress {
        address: as_str(field(value, "address")?)?.to_owned(),
        port: u16::try_from(port).map_err(|_| ParseError::Overflow(port))?,
        score: as_i64(field(value, "score")?)?,
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
        // A lifetime receipts flow, delivered pre-summed by the validator; not
        // supply-bounded, so it lands in the flow-sum type through its
        // boundary door rather than being rejected by the amount bound.
        received: match opt_field(value, "received") {
            Some(v) => ZatoshisFlowSum::from_summed(as_u64(v)?),
            None => ZatoshisFlowSum::from_summed(0),
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
                satoshis: SignedZatoshis::try_new(as_i64(field(d, "satoshis")?)?)
                    .map_err(|e| ParseError::Amount(e.to_string()))?,
                txid: as_txid(field(d, "txid")?)?,
                index: as_u32(field(d, "index")?)?,
                height: as_height(field(d, "height")?)?,
                address: TransparentAddress::new(as_str(field(d, "address")?)?.to_owned()),
                // the legacy full node emits `blockindex`; a validator that does not is
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

/// Parse a `getrawmempool verbose` response: a map of txid to its entry object.
///
/// Zebra reports more per entry than this takes (descendant stats, dependency
/// lists); `size`, `fee`, `height` and `time` are read because the node-RPC
/// mempool listing serves each of them, and the rest is ignored because parsing
/// a field commits us to its shape. `fee` arrives as a ZEC number and is
/// converted to zatoshis from its decimal text (see [`parse_fee`]).
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
                size: as_u64(field(meta, "size")?)?,
                fee: parse_fee(field(meta, "fee")?)?,
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
        sapling: pool_root::<sapling_crypto::Node>(opt_field(value, "sapling"), |r| r.to_bytes())?,
        // Orchard and Ironwood share a node type and a root representation, so
        // they share this reader — they differ only in which field they read.
        orchard: pool_root::<zebra_chain::orchard::tree::Node>(opt_field(value, "orchard"), |r| {
            r.to_repr()
        })?,
        ironwood: pool_root::<zebra_chain::orchard::tree::Node>(
            opt_field(value, "ironwood"),
            |r| r.to_repr(),
        )?,
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

/// Read one pool's final-state tree and turn its root into a [`TreeRootInfo`].
///
/// The pools differ only in the tree node type `N` and in how that node's root
/// is turned into its 32 bytes, so `root_bytes` supplies that last step per
/// pool (`Node::to_bytes` for Sapling, `Node::to_repr` for Orchard/Ironwood).
fn pool_root<N>(
    pool: Option<&serde_json::Value>,
    root_bytes: impl FnOnce(N) -> [u8; 32],
) -> Result<Option<TreeRootInfo>, ParseError>
where
    N: incrementalmerkletree::Hashable + Clone + zcash_primitives::merkle_tree::HashSer,
{
    let Some(bytes) = pool_final_state(pool)? else {
        return Ok(None);
    };
    let tree = read_tree::<N>(&bytes)?;
    Ok(Some(TreeRootInfo {
        root: TreeRoot::new(root_bytes(tree.root())),
        size: tree_size(tree.size())?,
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

/// Parse chainwork as a validator reports it, where the two validators
/// disagree on both the encoding and whether they track it at all.
///
/// The legacy full node sends a hex string. Zebra types the field as a 64-bit
/// integer, so it arrives as a JSON number, and hardcodes it to zero because
/// it does not store cumulative work per height. Both encodings land on the
/// same door, [`AbsoluteChainWork::try_from_reported`], which owns the reported-value
/// semantics: all-zero reads as `None` — "not reported", never a zero a
/// consumer could compare — and a value past the domain's 128-bit width is
/// refused rather than truncated.
fn parse_reported_chain_work(
    value: &serde_json::Value,
) -> Result<Option<AbsoluteChainWork>, ParseError> {
    let be = if let Some(number) = value.as_u64() {
        let mut be = [0u8; 32];
        be[24..].copy_from_slice(&number.to_be_bytes());
        be
    } else {
        chain_work_be_bytes(value)?
    };
    AbsoluteChainWork::try_from_reported(be).map_err(ParseError::AbsoluteChainWork)
}

/// Cumulative chainwork as a hex string, decoded to the wire's 32 big-endian
/// bytes. Natural order, and left-padded rather than fixed width: it is a
/// big-endian integer, so validators trim leading zeroes and an early-chain
/// response is genuinely short rather than malformed. Anything longer than 32
/// bytes is out of range for the protocol and is rejected.
fn chain_work_be_bytes(value: &serde_json::Value) -> Result<[u8; 32], ParseError> {
    let s = as_str(value)?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    let padded = format!("{s:0>64}");
    let bytes = hex::decode(&padded).map_err(|e| ParseError::Hex(e.to_string()))?;
    bytes
        .try_into()
        .map_err(|b: Vec<u8>| ParseError::WrongLength {
            expected: 32,
            got: b.len(),
        })
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
            .map(SignedZatoshis::try_new)
            .transpose()
            .map_err(|e| ParseError::Amount(e.to_string()))?,
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
        final_sapling_root: opt_field(value, "finalsaplingroot")
            .map(as_tree_root)
            .transpose()?,
        final_orchard_root: opt_field(value, "finalorchardroot")
            .map(as_tree_root)
            .transpose()?,
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
fn pool_tree_size(trees: Option<&serde_json::Value>, pool: &str) -> Result<TreeSize, ParseError> {
    let Some(size) = trees
        .and_then(|t| t.get(pool))
        .and_then(|p| opt_field(p, "size"))
    else {
        return Ok(TreeSize::ZERO);
    };
    Ok(TreeSize::try_from(as_u64(size)?)?)
}

/// A deserialised tree's `usize` note count, as a [`TreeSize`].
fn tree_size(count: usize) -> Result<TreeSize, ParseError> {
    let count = u64::try_from(count)
        .map_err(|_| ParseError::Deserialize(format!("tree size {count} does not fit u64")))?;
    Ok(TreeSize::try_from(count)?)
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
                        satoshis: SignedZatoshis::try_new(as_i64(field(i, "satoshis")?)?)
                            .map_err(|e| ParseError::Amount(e.to_string()))?,
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

    /// `parse_network_info` reads the zebra 6.4.2 oracle's `getnetworkinfo` into
    /// the domain type: the identity fields, the per-network reachability, and
    /// the relay fee converted from its ZEC float to exact zatoshis (1e-6 ZEC =
    /// 100 zat).
    #[test]
    fn network_info_parses_the_oracle_response() {
        let value = json!({
            "version": 6_040_200,
            "subversion": "/Zebra:6.4.2/",
            "protocolversion": 170_160,
            "localservices": "0000000000000001",
            "timeoffset": 0,
            "connections": 44,
            "networks": [
                {"name": "ipv4", "limited": false, "reachable": true, "proxy": "", "proxy_randomize_credentials": false},
                {"name": "ipv6", "limited": false, "reachable": true, "proxy": "", "proxy_randomize_credentials": false},
                {"name": "onion", "limited": false, "reachable": false, "proxy": "", "proxy_randomize_credentials": false}
            ],
            "relayfee": 1e-6,
            "localaddresses": [],
            "warnings": ""
        });
        let info = parse_network_info(&value).expect("parses");
        assert_eq!(info.version, 6_040_200);
        assert_eq!(info.subversion, "/Zebra:6.4.2/");
        assert_eq!(info.protocol_version, 170_160);
        assert_eq!(info.local_services, "0000000000000001");
        assert_eq!(info.time_offset, 0);
        assert_eq!(info.connections, 44);
        assert_eq!(info.networks.len(), 3);
        assert_eq!(info.networks[2].name, "onion");
        assert!(!info.networks[2].reachable);
        assert_eq!(info.relay_fee, Zatoshis::new(100).expect("valid amount"));
        assert!(info.local_addresses.is_empty());
        assert_eq!(info.warnings, "");
    }

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
        let trimmed = parse_reported_chain_work(&json!("ff")).expect("short chainwork");

        assert_eq!(
            trimmed,
            Some(AbsoluteChainWork::new(
                core::num::NonZeroU128::new(0xff).expect("nonzero")
            ))
        );
    }

    /// Zero off the wire — either validator's encoding — is "not reported",
    /// not a comparable amount of work.
    #[test]
    fn chainwork_zero_reads_as_not_reported() {
        assert_eq!(
            parse_reported_chain_work(&json!("00")).expect("valid"),
            None
        );
        assert_eq!(parse_reported_chain_work(&json!(0)).expect("valid"), None);
    }

    /// Chainwork past the domain's 128-bit width is refused at parse rather
    /// than truncated into a lower — and wrongly ordered — value.
    #[test]
    fn chainwork_over_width_is_refused() {
        let over = format!("01{}", "00".repeat(31));
        assert!(matches!(
            parse_reported_chain_work(&json!(over)),
            Err(ParseError::AbsoluteChainWork(_))
        ));
    }

    /// A reported tree size is accepted up to `u32::MAX` and a full depth-32
    /// tree (`2^32`) is refused at parse rather than stored as a wrapped value
    /// (issue #549).
    #[test]
    fn tree_size_past_u32_is_refused() {
        let trees = |size: u64| json!({ "sapling": { "size": size } });

        let max = u64::from(u32::MAX);
        assert_eq!(
            pool_tree_size(Some(&trees(max)), "sapling").expect("u32::MAX fits"),
            TreeSize::from(u32::MAX)
        );

        let full = 1_u64 << 32;
        assert!(matches!(
            pool_tree_size(Some(&trees(full)), "sapling"),
            Err(ParseError::TreeSize(TreeSizeOutOfRange { got })) if got == full
        ));
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
        assert_eq!(
            sapling.size,
            TreeSize::ZERO,
            "an empty tree holds no commitments"
        );
        let orchard = roots.orchard.expect("orchard pool present");
        assert_eq!(orchard.size, TreeSize::ZERO);
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
            sapling.size,
            TreeSize::ZERO,
            "size comes from the tree, and there is no tree here"
        );
    }

    /// `parse_treestate` discards zebra's `finalRoot` even when the reply carries
    /// one: the RPC path does not trust the field (zebra documents it as unused),
    /// so the domain carries `None`, and the node-RPC wire then omits `finalRoot`
    /// — a recorded divergence. The serialized tree (`finalState`) is kept.
    #[test]
    fn parse_treestate_discards_the_final_root() {
        let value = json!({
            "hash": "ab".repeat(32),
            "height": 100,
            "time": 1_600_000_000,
            "sapling": { "commitments": { "finalRoot": "cd".repeat(32), "finalState": "dead" } },
        });
        let treestate = parse_treestate(&value).expect("parses");
        let sapling = treestate.sapling.expect("the pool is present");
        assert!(
            sapling.final_root.is_none(),
            "finalRoot is discarded on parse, not carried through"
        );
        assert_eq!(
            sapling.final_state,
            hex::decode("dead").expect("valid hex"),
            "finalState is kept"
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

    /// The verbose listing carries more per entry than Zaino reads, but the
    /// node-RPC mempool view serves `size`, `fee`, `height` and `time`, so all
    /// four are taken. The `fee` is a ZEC number and reaches the domain as its
    /// exact zatoshi integer.
    #[test]
    fn verbose_mempool_takes_size_fee_height_and_time() {
        let value = json!({
            ASYMMETRIC_HEX: {
                "size": 1_234,
                "fee": 0.00001,
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
        assert_eq!(entries[0].size, 1_234);
        assert_eq!(
            entries[0].fee,
            Zatoshis::new(1_000).expect("valid"),
            "0.00001 ZEC is 1000 zatoshis, from the decimal text"
        );
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
        let without_time = json!({
            ASYMMETRIC_HEX: { "height": 2_500_000, "size": 1_234, "fee": 0.00001 },
        });
        let entries = parse_mempool_metadata(&without_time).expect("height alone parses");
        assert_eq!(entries[0].entry_time, None);

        let without_height =
            json!({ ASYMMETRIC_HEX: { "time": 1_700_000_000i64, "size": 1_234, "fee": 0.00001 } });
        assert!(matches!(
            parse_mempool_metadata(&without_height),
            Err(ParseError::MissingField("height"))
        ));
    }

    /// The fee conversion is exact across the forms serde renders a JSON number
    /// in — a plain decimal, an integer, the supply ceiling, and the scientific
    /// notation serde uses for a lone zatoshi — with no `f64` multiply.
    #[test]
    fn a_fee_is_converted_exactly_from_its_decimal_text() {
        let cases = [
            (json!(0.00001), 1_000u64),
            (json!(0.00000001), 1),
            (json!(1), 100_000_000),
            (json!(1.5), 150_000_000),
            (json!(0), 0),
            (json!(21_000_000), 2_100_000_000_000_000),
        ];
        for (fee, expected) in cases {
            let value = json!({ ASYMMETRIC_HEX: { "height": 1, "size": 1, "fee": fee } });
            let entries = parse_mempool_metadata(&value).expect("parses");
            assert_eq!(
                entries[0].fee,
                Zatoshis::new(expected).expect("valid"),
                "{fee} ZEC should be {expected} zatoshis"
            );
        }
    }

    /// A fee that is not a number, or carries detail below one zatoshi, is a
    /// typed error — never a silently rounded or defaulted amount.
    #[test]
    fn a_malformed_fee_is_a_typed_error() {
        let not_a_number = json!({ ASYMMETRIC_HEX: { "height": 1, "size": 1, "fee": "abc" } });
        assert!(matches!(
            parse_mempool_metadata(&not_a_number),
            Err(ParseError::UnexpectedType { .. })
        ));

        let too_precise = json!({ ASYMMETRIC_HEX: { "height": 1, "size": 1, "fee": 0.000000001 } });
        assert!(
            matches!(
                parse_mempool_metadata(&too_precise),
                Err(ParseError::ZecAmount(ZecAmountError::TooPrecise { .. }))
            ),
            "a fee finer than one zatoshi is refused, not rounded"
        );
    }

    /// A fee above the money supply is out of range, not a wrapped amount. An
    /// integer one zatoshi-worth of ZEC over the ceiling fails, and so does the
    /// finest fractional step past it.
    #[test]
    fn a_fee_above_the_supply_is_out_of_range() {
        for fee in [json!(21_000_001), json!(21_000_000.000_000_01f64)] {
            let value = json!({ ASYMMETRIC_HEX: { "height": 1, "size": 1, "fee": fee } });
            assert!(
                matches!(
                    parse_mempool_metadata(&value),
                    Err(ParseError::ZecAmount(ZecAmountError::OutOfRange { .. }))
                ),
                "{fee} ZEC is above the supply and must be rejected"
            );
        }
    }

    /// An absurd exponent overflows the scale rather than wrapping or panicking.
    /// These texts cannot arrive through a `serde_json` number (an f64 cannot
    /// hold `1e400`), so the converter is exercised directly, which is the unit
    /// that must stay loud.
    #[test]
    fn an_absurd_exponent_is_out_of_range() {
        for text in ["1e400", "1e-400"] {
            assert!(
                matches!(
                    zec_text_to_zatoshis(text),
                    Err(ZecAmountError::OutOfRange { .. })
                ),
                "{text} must be out of range, not a wrapped or panicking value"
            );
        }
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
