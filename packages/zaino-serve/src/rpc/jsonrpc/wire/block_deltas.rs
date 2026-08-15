//! Types associated with the `getblockdeltas` RPC request.

use zebra_chain::amount::{Amount, NonNegative};

use crate::rpc::jsonrpc::wire::display_hex;

/// A delta amount that does not fit the wire's amount type.
///
/// # Why this is the one fallible `from_domain` in the module
///
/// Response rendering is otherwise infallible: a domain value is already valid,
/// so emitting it cannot fail. That does not quite hold here.
/// [`SignedZatoshis`](zaino_primitives::types::SignedZatoshis) is an unbounded
/// `i64`, while the wire's `Amount` enforces the money range — so the domain
/// type is strictly wider than what can be rendered, and the gap has to be
/// reported rather than papered over.
///
/// The old conversion in the source layer was fallible for exactly this reason;
/// keeping it fallible preserves that behaviour rather than trading it for a
/// panic or a silent clamp. Bounding `SignedZatoshis` at construction would
/// close the gap in the domain instead and let this become infallible, but that
/// is a change to the primitive's contract and belongs in its own commit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("block delta amount out of range: {0}")]
pub struct DeltaAmountOutOfRange(String);

impl BlockDeltas {
    /// Renders the domain type as the served JSON shape.
    ///
    /// Fails only if a delta amount falls outside the money range — see
    /// [`DeltaAmountOutOfRange`].
    pub fn from_domain(
        deltas: zaino_primitives::types::rpc::BlockDeltas,
    ) -> Result<Self, DeltaAmountOutOfRange> {
        fn amount<C: zebra_chain::amount::Constraint>(
            zats: i64,
        ) -> Result<Amount<C>, DeltaAmountOutOfRange> {
            Amount::try_from(zats).map_err(|e| DeltaAmountOutOfRange(e.to_string()))
        }

        Ok(Self {
            hash: display_hex(deltas.hash.into()),
            confirmations: deltas.confirmations,
            size: deltas.size as i64,
            height: deltas.height.into(),
            version: deltas.version,
            merkle_root: display_hex(deltas.merkle_root.into()),
            time: i64::from(deltas.time),
            median_time: i64::from(deltas.median_time),
            nonce: hex::encode(deltas.nonce),
            bits: format!("{:08x}", deltas.bits),
            difficulty: deltas.difficulty,
            previous_block_hash: deltas.previous_block_hash.map(|h| display_hex(h.into())),
            next_block_hash: deltas.next_block_hash.map(|h| display_hex(h.into())),
            deltas: deltas
                .deltas
                .into_iter()
                .map(|delta| {
                    Ok(BlockDelta {
                        txid: display_hex(delta.txid.into()),
                        index: delta.index,
                        inputs: delta
                            .inputs
                            .into_iter()
                            .map(|input| {
                                Ok(InputDelta {
                                    address: String::from(input.address),
                                    satoshis: amount(i64::from(input.satoshis))?,
                                    index: input.index,
                                    prevtxid: display_hex(input.prev_txid.into()),
                                    prevout: input.prev_output,
                                })
                            })
                            .collect::<Result<Vec<_>, DeltaAmountOutOfRange>>()?,
                        outputs: delta
                            .outputs
                            .into_iter()
                            .map(|output| {
                                // `Zatoshis` is already bounded by the money
                                // range, so the only step that can fail is the
                                // widening to `i64` — which cannot, given that
                                // bound. It is still checked rather than
                                // asserted, so the two amount paths read alike.
                                Ok(OutputDelta {
                                    address: String::from(output.address),
                                    satoshis: amount(
                                        i64::try_from(u64::from(output.satoshis))
                                            .map_err(|e| DeltaAmountOutOfRange(e.to_string()))?,
                                    )?,
                                    index: output.index,
                                })
                            })
                            .collect::<Result<Vec<_>, DeltaAmountOutOfRange>>()?,
                    })
                })
                .collect::<Result<Vec<_>, DeltaAmountOutOfRange>>()?,
        })
    }
}

/// Response to a `getblockdeltas` RPC request.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct BlockDeltas {
    /// The hash of the block.
    pub hash: String,

    /// The number of confirmations.
    pub confirmations: i64,

    /// Serialized block size in bytes.
    pub size: i64,

    /// Block height in the best chain.
    pub height: u32,

    /// Block header version.
    pub version: u32,

    /// The merkle root of the block.
    #[serde(rename = "merkleroot")]
    pub merkle_root: String,

    /// Per-transaction transparent deltas for this block.
    /// Each entry corresponds to a transaction at position `index` in the block and
    /// contains:
    /// - `inputs`: non-coinbase vins with **negative** zatoshi amounts and their prevouts,
    /// - `outputs`: vouts with exactly one transparent address and **positive** amounts.
    pub deltas: Vec<BlockDelta>,

    /// Block header timestamp as set by the miner.
    pub time: i64,

    /// Median-Time-Past (MTP) of this block, i.e. the median of the timestamps of
    /// this block and up to the 10 previous blocks `[N-10 … N]` (Unix epoch seconds).
    #[serde(rename = "mediantime")]
    pub median_time: i64,

    /// Block header nonce encoded as hex (Equihash nonce).
    pub nonce: String,

    /// Compact target (“nBits”) as a hex string, e.g. `"1d00ffff"`.
    pub bits: String,

    /// Difficulty corresponding to `bits` (relative to minimum difficulty, e.g. `1.0`).
    pub difficulty: f64,

    // `chainwork` would be here, but Zebra does not plan to support it
    // pub chainwork: Vec<u8>,
    /// Previous block hash as hex, or `None` for genesis.
    #[serde(skip_serializing_if = "Option::is_none", rename = "previousblockhash")]
    pub previous_block_hash: Option<String>,

    /// Next block hash in the active chain, if known. Omitted for the current tip
    /// or for blocks not in the active chain.
    #[serde(skip_serializing_if = "Option::is_none", rename = "nextblockhash")]
    pub next_block_hash: Option<String>,
}

/// Per-transaction transparent deltas within a block, as returned by
/// `getblockdeltas`. One `BlockDelta` is emitted for each transaction in
/// the block, at the transaction’s position (`index`).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct BlockDelta {
    /// Transaction hash.
    pub txid: String,

    /// Zero-based position of this transaction within the block.
    pub index: u32,

    /// Transparent input deltas (non-coinbase only).
    ///
    /// Each entry spends a previous transparent output and records a **negative**
    /// amount in zatoshis. Inputs that do not resolve to exactly one transparent
    /// address are omitted.
    pub inputs: Vec<InputDelta>,

    /// Transparent output deltas.
    ///
    /// Each entry pays exactly one transparent address and records a **positive**
    /// amount in zatoshis. Outputs without a single transparent address (e.g.,
    /// OP_RETURN, bare multisig with multiple addresses) are omitted.
    pub outputs: Vec<OutputDelta>,
}

/// A single transparent input delta within a transaction.
///
/// Represents spending of a specific previous output (`prevtxid`/`prevout`)
/// to a known transparent address. Amounts are **negative** (funds leaving).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputDelta {
    /// Transparent address that the spent prevout paid to.
    pub address: String,

    /// Amount in zatoshis, **negative** for inputs/spends.
    pub satoshis: Amount,

    /// Zero-based vin index within the transaction.
    pub index: u32,

    /// Hash of the previous transaction containing the spent output.
    pub prevtxid: String,

    /// Output index (`vout`) in `prevtxid` that is being spent.
    pub prevout: u32,
}

/// A single transparent output delta within a transaction.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OutputDelta {
    /// Transparent address paid by this output.
    pub address: String,

    /// Amount in zatoshis, **non-negative**.
    pub satoshis: Amount<NonNegative>,

    /// Zero-based vout index within the transaction.
    pub index: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{
        self as domain, Height, SignedZatoshis, TransparentAddress, Zatoshis,
    };

    /// Asymmetric under reversal, so a missing or doubled byte-reversal shows up.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    fn sample() -> domain::rpc::BlockDeltas {
        domain::rpc::BlockDeltas {
            hash: domain::BlockHash::from(ASYMMETRIC),
            confirmations: 7,
            size: 1_234,
            height: Height::try_from(42u32).unwrap(),
            version: 4,
            merkle_root: domain::MerkleRoot::from([0xaa; 32]),
            deltas: vec![domain::rpc::BlockDelta {
                txid: domain::TransactionId::from(ASYMMETRIC),
                index: 1,
                inputs: vec![domain::rpc::InputDelta {
                    address: TransparentAddress::new("t1spender".to_string()),
                    satoshis: SignedZatoshis::new(-5_000),
                    index: 0,
                    prev_txid: domain::TransactionId::from([0xbb; 32]),
                    prev_output: 3,
                }],
                outputs: vec![domain::rpc::OutputDelta {
                    address: TransparentAddress::new("t1payee".to_string()),
                    satoshis: Zatoshis::new(5_000).unwrap(),
                    index: 0,
                }],
            }],
            time: 1_700_000_000,
            median_time: 1_699_999_000,
            nonce: [0xcc; 32],
            bits: 0x1d00_ffff,
            difficulty: 1.0,
            previous_block_hash: Some(domain::BlockHash::from([0xdd; 32])),
            next_block_hash: None,
        }
    }

    /// Pins zcashd's field names and encodings. A rename here is a wire break.
    #[test]
    fn renders_zcashd_field_names_and_encodings() {
        let wire = BlockDeltas::from_domain(sample()).expect("amounts in range");
        let json = serde_json::to_value(&wire).unwrap();

        let mut display_order = ASYMMETRIC;
        display_order.reverse();
        assert_eq!(json["hash"], hex::encode(display_order));
        assert_eq!(json["deltas"][0]["txid"], hex::encode(display_order));

        // Nonces are opaque header bytes, not identifiers: not reversed.
        assert_eq!(json["nonce"], hex::encode([0xcc; 32]));
        assert_eq!(json["bits"], "1d00ffff");
        assert_eq!(json["mediantime"], 1_699_999_000i64);
        assert_eq!(json["merkleroot"], hex::encode([0xaa; 32]));
        assert_eq!(json["previousblockhash"], hex::encode([0xdd; 32]));
        assert!(
            json.get("nextblockhash").is_none(),
            "an absent next block is omitted, not null"
        );

        // A spend is negative and a payment positive, in zatoshis either way.
        assert_eq!(json["deltas"][0]["inputs"][0]["satoshis"], -5_000i64);
        assert_eq!(json["deltas"][0]["inputs"][0]["prevout"], 3);
        assert_eq!(json["deltas"][0]["outputs"][0]["satoshis"], 5_000i64);
    }

    #[test]
    fn rejects_an_input_amount_outside_the_money_range() {
        let mut deltas = sample();
        deltas.deltas[0].inputs[0].satoshis = SignedZatoshis::new(i64::MIN);

        assert!(
            BlockDeltas::from_domain(deltas).is_err(),
            "an unrepresentable amount must be reported, not clamped or panicked on"
        );
    }
}
