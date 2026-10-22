//! JSON-RPC `result` → zaino-primitives types, one parser per method
//!
//! - Only fields Zaino reads are parsed: an unread field never fails a poll
//! - Absent / `null` optional field = `None`
//! - Present but malformed = error ("inactive" != "garbled")

use zaino_primitives::types::{BlockRef, TransactionLocation};

use zcash_protocol::consensus::{BlockHeight, BranchId, UPGRADES_IN_ORDER};
use zcash_protocol::value::{Zatoshis, MAX_MONEY};
use zcash_protocol::{BlockHash, TxId};

use crate::{
    BlockchainInfo, EndOfService, MempoolListed, NodeRelease, PeerInfo, TransactionResponse,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    #[error("hex decode: {0}")]
    Hex(String),
    #[error("not a block hash (64 hex digits): {0}")]
    Hash(String),
    #[error("not a txid (64 hex digits): {0}")]
    Txid(String),
    #[error("expected {expected}, got {got}")]
    UnexpectedType { expected: &'static str, got: String },
    #[error("value {0} overflows target type")]
    Overflow(u64),
    #[error("missing field `{0}`")]
    MissingField(&'static str),
    #[error("mempool listing too large: {len} entries > {max}")]
    ListingTooLarge { len: usize, max: usize },
    #[error("ZEC amount {0} outside the money supply")]
    Zec(f64),
}

impl ParseError {
    fn unexpected(expected: &'static str, value: &serde_json::Value) -> Self {
        let got = format!("{value}").chars().take(64).collect();
        Self::UnexpectedType { expected, got }
    }
}

/// Required field (the error names it, not the whole object)
fn field<'a>(
    value: &'a serde_json::Value,
    name: &'static str,
) -> Result<&'a serde_json::Value, ParseError> {
    value.get(name).ok_or(ParseError::MissingField(name))
}

/// Omitted and `null` alike = absent
fn opt_field<'a>(
    value: &'a serde_json::Value,
    name: &'static str,
) -> Option<&'a serde_json::Value> {
    value.get(name).filter(|v| !v.is_null())
}

fn as_str(value: &serde_json::Value) -> Result<&str, ParseError> {
    value.as_str().ok_or_else(|| ParseError::unexpected("string", value))
}

fn as_u64(value: &serde_json::Value) -> Result<u64, ParseError> {
    value.as_u64().ok_or_else(|| ParseError::unexpected("u64", value))
}

fn as_i64(value: &serde_json::Value) -> Result<i64, ParseError> {
    value.as_i64().ok_or_else(|| ParseError::unexpected("i64", value))
}

fn as_bool(value: &serde_json::Value) -> Result<bool, ParseError> {
    value.as_bool().ok_or_else(|| ParseError::unexpected("bool", value))
}

fn as_array(value: &serde_json::Value) -> Result<&Vec<serde_json::Value>, ParseError> {
    value.as_array().ok_or_else(|| ParseError::unexpected("array", value))
}

fn as_height(value: &serde_json::Value) -> Result<BlockHeight, ParseError> {
    let height = as_u64(value)?;
    BlockHeight::try_from(height).map_err(|_| ParseError::Overflow(height))
}

fn hex(value: &serde_json::Value) -> Result<Vec<u8>, ParseError> {
    const_hex::decode(as_str(value)?).map_err(|e| ParseError::Hex(e.to_string()))
}

pub(crate) fn as_txid(value: &serde_json::Value) -> Result<TxId, ParseError> {
    txid(as_str(value)?)
}

fn txid(hex: &str) -> Result<TxId, ParseError> {
    TxId::from_hex(hex).ok_or_else(|| ParseError::Txid(hex.chars().take(64).collect()))
}

pub(crate) fn as_block_hash(value: &serde_json::Value) -> Result<BlockHash, ParseError> {
    let hex = as_str(value)?;
    BlockHash::from_hex(hex).ok_or_else(|| ParseError::Hash(hex.chars().take(64).collect()))
}

/// Hex-string `result` decoded straight off the body: one SIMD pass into an exact-size buffer
///
/// - `deserialize_bytes`: serde_json hands the string's raw bytes (no UTF-8 pass)
/// - Non-hex byte = hex error
pub(crate) struct HexBytes(pub(crate) Vec<u8>);

impl<'de> serde::Deserialize<'de> for HexBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = HexBytes;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a hex string")
            }

            fn visit_bytes<E: serde::de::Error>(self, hex: &[u8]) -> Result<HexBytes, E> {
                const_hex::decode(hex).map(HexBytes).map_err(E::custom)
            }

            fn visit_str<E: serde::de::Error>(self, hex: &str) -> Result<HexBytes, E> {
                self.visit_bytes(hex.as_bytes())
            }
        }

        deserializer.deserialize_bytes(Visitor)
    }
}

/// `getblockchaininfo`: tip, estimate, each known upgrade's activation height (keyed by branch id)
///
/// - Branch id unknown to this build = skipped
/// - Every other field ignored
/// - Activations in protocol order (a JSON object carries none)
pub(crate) fn parse_blockchain_info(
    value: &serde_json::Value,
) -> Result<BlockchainInfo, ParseError> {
    let upgrades = field(value, "upgrades")?;
    let upgrades =
        upgrades.as_object().ok_or_else(|| ParseError::unexpected("object", upgrades))?;
    let mut activations = Vec::new();
    for (key, upgrade) in upgrades {
        let known = u32::from_str_radix(key, 16).ok().and_then(|id| BranchId::try_from(id).ok());
        if let Some(branch) = known {
            activations.push((branch, as_height(field(upgrade, "activationheight")?)?));
        }
    }
    activations.sort_by_key(|(branch, _)| {
        UPGRADES_IN_ORDER.iter().position(|upgrade| upgrade.branch_id() == *branch)
    });
    let tip = BlockRef {
        hash: as_block_hash(field(value, "bestblockhash")?)?,
        height: as_height(field(value, "blocks")?)?,
    };
    let estimated_height = as_height(field(value, "estimatedheight")?)?;
    Ok(BlockchainInfo { tip, estimated_height, activations })
}

/// Entries accepted from one `getrawmempool` (ZIP-401 node holds ~8k)
///
/// - Trips only on a hostile node, before a million raw-transaction fetches follow
const MAX_MEMPOOL_LISTING_ENTRIES: usize = 1_000_000;

/// `getrawmempool true` (`{txid: {fee, ..}}`), refused on its count before any entry is decoded
pub(crate) fn parse_mempool_listing(
    value: &serde_json::Value,
) -> Result<Vec<MempoolListed>, ParseError> {
    let entries = value.as_object().ok_or_else(|| ParseError::unexpected("object", value))?;
    if entries.len() > MAX_MEMPOOL_LISTING_ENTRIES {
        return Err(ParseError::ListingTooLarge {
            len: entries.len(),
            max: MAX_MEMPOOL_LISTING_ENTRIES,
        });
    }
    entries
        .iter()
        .map(|(txid, entry)| {
            let size = field(entry, "size")?;
            Ok(MempoolListed {
                txid: self::txid(txid)?,
                fee: as_zec(field(entry, "fee")?)?,
                encoded_len: size
                    .as_u64()
                    .and_then(|size| u32::try_from(size).ok())
                    .ok_or_else(|| ParseError::unexpected("u32", size))?,
            })
        })
        .collect()
}

/// Decimal ZEC (zebra's `f64`) → zatoshis
///
/// - Exact: every in-supply count < 2^53, so `zats / 1e8` then `× 1e8` lands within 0.25 zat
fn as_zec(value: &serde_json::Value) -> Result<Zatoshis, ParseError> {
    let zec = value.as_f64().ok_or_else(|| ParseError::unexpected("number", value))?;
    let zats = (zec * 100_000_000.0).round();
    if !(0.0..=MAX_MONEY as f64).contains(&zats) {
        return Err(ParseError::Zec(zec));
    }
    Zatoshis::from_u64(zats as u64).map_err(|_| ParseError::Zec(zec))
}

/// `getrawtransaction <txid> 1`: `height` absent = mempool, `-1` = side chain, `>= 0` = best chain
///
/// - Any other negative refused (undefined, never folded into side chain)
pub(crate) fn parse_transaction(
    value: &serde_json::Value,
) -> Result<TransactionResponse, ParseError> {
    let location = match opt_field(value, "height").map(as_i64).transpose()? {
        None => TransactionLocation::Mempool,
        Some(-1) => TransactionLocation::NonBestChain,
        Some(_) => TransactionLocation::BestChain(as_height(field(value, "height")?)?),
    };
    Ok(TransactionResponse { bytes: hex(field(value, "hex")?)?, location })
}

/// `getinfo` + `getdeprecationinfo` (`None` = the release lacks the latter)
///
/// - `end_of_service` absent = not enforced on this network
pub(crate) fn parse_node_release(
    info: &serde_json::Value,
    deprecation: Option<&serde_json::Value>,
) -> Result<NodeRelease, ParseError> {
    let end_of_service = match deprecation {
        None => EndOfService::Unknown,
        Some(deprecation) => match opt_field(deprecation, "end_of_service") {
            None => EndOfService::NotEnforced,
            Some(at) => EndOfService::At { height: as_height(field(at, "block_height")?)? },
        },
    };
    Ok(NodeRelease {
        build: as_str(field(info, "build")?)?.to_owned(),
        user_agent: as_str(field(info, "subversion")?)?.to_owned(),
        end_of_service,
    })
}

/// `getpeerinfo`
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Reads differently forwards and backwards (a mirrored decode cannot pass by coincidence)
    const ASYMMETRIC_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddee01";

    /// Zebra's verbose listing (`fee` = lossy `f64` ZEC)
    ///
    /// - key reversed → txid
    /// - fee → exact zatoshis (ZIP-317 minimum, sub-zat noise, whole supply)
    /// - `size` = encoded length
    /// - refused: negative / past-supply fee, missing fee or size, size past `u32`, over-cap
    ///   listing
    #[test]
    fn mempool_listing_reads_each_txid_and_its_exact_fee() {
        let mut reversed_bytes = [0u8; 32];
        const_hex::decode_to_slice(ASYMMETRIC_HEX, &mut reversed_bytes).expect("fixture");
        reversed_bytes.reverse();
        let listed = |fee: serde_json::Value| {
            parse_mempool_listing(&json!({ ASYMMETRIC_HEX: { "size": 250, "fee": fee } }))
        };

        for (fee, zats) in [
            (json!(0.0001), 10_000),
            (json!(0.00015), 15_000),
            (json!(1e-8), 1),
            (json!(0.1 + 0.2), 30_000_000),
            (json!(21_000_000.0), 2_100_000_000_000_000),
            (json!(0), 0),
        ] {
            let txid = TxId::from_bytes(reversed_bytes);
            let fee_zats = Zatoshis::from_u64(zats).expect("in supply");
            let expected = MempoolListed { txid, fee: fee_zats, encoded_len: 250 };
            assert_eq!(listed(fee.clone()).expect("listing"), vec![expected], "fee {fee}");
        }
        for fee in [json!(-0.0001), json!(21_000_000.00000001 + 1.0), json!("0.0001")] {
            assert!(listed(fee.clone()).is_err(), "fee {fee}");
        }
        let no_fee = parse_mempool_listing(&json!({ ASYMMETRIC_HEX: { "size": 250 } }));
        assert!(matches!(no_fee, Err(ParseError::MissingField("fee"))));
        let no_size = parse_mempool_listing(&json!({ ASYMMETRIC_HEX: { "fee": 0.0001 } }));
        assert!(matches!(no_size, Err(ParseError::MissingField("size"))));
        let huge =
            parse_mempool_listing(&json!({ ASYMMETRIC_HEX: { "size": 1u64 << 32, "fee": 0 } }));
        assert!(huge.is_err());

        let oversized: serde_json::Map<String, serde_json::Value> = (0
            ..=MAX_MEMPOOL_LISTING_ENTRIES)
            .map(|n| (format!("{n:064x}"), json!({ "fee": 0.0001 })))
            .collect();
        let oversized = parse_mempool_listing(&serde_json::Value::Object(oversized));
        assert!(matches!(oversized, Err(ParseError::ListingTooLarge { .. })));
    }

    /// - Read: tip (hash reversed), estimate, each known upgrade's activation by branch id
    /// - Never fail the poll: unread fields (lossy pool floats: the old mainnet-boot crash), an
    ///   upgrade this build does not know (a validator ahead of the pin), names, statuses
    /// - A known upgrade without its height, a missing tip → parse fails
    #[test]
    fn blockchain_info_reads_tip_estimate_and_known_activations_and_ignores_the_rest() {
        let info = json!({
            "chain": "main",
            "blocks": 3_451_543,
            "estimatedheight": 3_451_544,
            "bestblockhash": ASYMMETRIC_HEX,
            "chainSupply": { "chainValue": "not even a number" },
            "valuePools": [{ "id": "sapling", "chainValue": 529_544.04149098 }],
            "upgrades": {
                "76b809bb": { "name": "Sapling", "activationheight": 419_200, "status": "active" },
                "c2d6d0b4": { "name": "NU5", "activationheight": 1_687_104, "status": "?" },
                "deadbeef": { "name": "NU9", "activationheight": 9_999_999, "status": "pending" },
            },
            "consensus": { "chaintip": "c2d6d0b4", "nextblock": "deadbeef" },
        });
        let mut reversed_bytes = [0u8; 32];
        const_hex::decode_to_slice(ASYMMETRIC_HEX, &mut reversed_bytes).expect("fixture");
        reversed_bytes.reverse();
        let height = BlockHeight::from;

        let parsed = parse_blockchain_info(&info).expect("well-formed");
        let tip = BlockRef { hash: BlockHash(reversed_bytes), height: height(3_451_543) };
        assert_eq!((parsed.tip, parsed.estimated_height), (tip, height(3_451_544)));
        let known = [(BranchId::Sapling, height(419_200)), (BranchId::Nu5, height(1_687_104))];
        assert_eq!(
            parsed.activations, known,
            "protocol order; NU9 (unknown to this build) skipped"
        );

        let mut no_height = info.clone();
        let nu5 = no_height["upgrades"]["c2d6d0b4"].as_object_mut().expect("object");
        nu5.remove("activationheight");
        let no_height = parse_blockchain_info(&no_height);
        assert!(matches!(no_height, Err(ParseError::MissingField("activationheight"))));
        let mut no_tip = info;
        no_tip.as_object_mut().expect("object").remove("bestblockhash");
        let no_tip = parse_blockchain_info(&no_tip);
        assert!(matches!(no_tip, Err(ParseError::MissingField("bestblockhash"))));
    }

    /// - All three placements pinned (side-chain tx reported as unmined = reads as pending)
    /// - Undefined negative height refused
    #[test]
    fn transactions_place_themselves() {
        use TransactionLocation::{BestChain, Mempool, NonBestChain};
        let location = |value| parse_transaction(&value).map(|tx| tx.location);
        let mined = location(json!({ "hex": "00", "height": 12345 })).expect("mined");
        let side = location(json!({ "hex": "00", "height": -1 })).expect("side chain");
        let mempool = location(json!({ "hex": "00" })).expect("mempool");
        let height = BlockHeight::from(12345);
        assert_eq!((mined, side, mempool), (BestChain(height), NonBestChain, Mempool));
        assert!(location(json!({ "hex": "00", "height": -7 })).is_err());
    }

    /// zebrad 6.4.2's shapes
    ///
    /// - mainnet → halt height
    /// - other networks → omitted
    /// - release before `getdeprecationinfo` → unknown (never read as "no halt")
    #[test]
    fn a_release_reads_its_build_and_where_it_halts() {
        let info = json!({
            "version": 6040250, "build": "v6.4.2", "subversion": "/Zebra:6.4.2/",
            "protocolversion": 170140, "blocks": 3_400_000, "connections": 8,
        });
        let release = |deprecation: Option<serde_json::Value>| {
            parse_node_release(&info, deprecation.as_ref()).map(|release| release.end_of_service)
        };
        let mainnet = json!({ "end_of_service": { "block_height": 3_564_960, "estimated_time": 1_790_000_000 } });
        let halts = EndOfService::At { height: BlockHeight::from(3_564_960) };
        assert_eq!(release(Some(mainnet)).expect("mainnet"), halts);
        assert_eq!(release(Some(json!({}))).expect("testnet"), EndOfService::NotEnforced);
        assert_eq!(release(None).expect("zebrad 6.2"), EndOfService::Unknown);

        let read = parse_node_release(&info, None).expect("getinfo");
        assert_eq!((read.build.as_str(), read.user_agent.as_str()), ("v6.4.2", "/Zebra:6.4.2/"));
        let no_height = json!({ "end_of_service": { "estimated_time": 1 } });
        let missing = parse_node_release(&info, Some(&no_height));
        assert!(matches!(missing, Err(ParseError::MissingField("block_height"))));
    }
}
