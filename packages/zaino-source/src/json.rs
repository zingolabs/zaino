//! JSON-RPC `result` → zaino-primitives types, one serde reply shape per method
//!
//! - Only fields Zaino reads are declared: an unread field never fails a poll
//! - Absent / `null` optional field = `None`
//! - Present but malformed = error ("inactive" != "garbled")

use std::collections::BTreeMap;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use zaino_primitives::types::{BlockRef, TransactionLocation};
use zcash_protocol::consensus::{BlockHeight, BranchId, UPGRADES_IN_ORDER};
use zcash_protocol::value::{Zatoshis, MAX_MONEY};
use zcash_protocol::{BlockHash, TxId};

use crate::{
    BlockchainInfo, EndOfService, MempoolListed, NodeRelease, PeerInfo, TransactionResponse,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("mempool listing too large: {len} entries > {max}")]
    ListingTooLarge { len: usize, max: usize },
    #[error("height {0}: neither -1 nor a block height")]
    Height(i64),
    #[error("ZEC amount {0} outside the money supply")]
    Zec(f64),
}

/// 64 hex digits, display order (a bare `result` too: `block_hash(value)`)
pub(crate) fn block_hash<'de, D: Deserializer<'de>>(hex: D) -> Result<BlockHash, D::Error> {
    let hex = String::deserialize(hex)?;
    let invalid = || D::Error::custom(format!("not a block hash (64 hex digits): {hex:.64}"));
    BlockHash::from_hex(&hex).ok_or_else(invalid)
}

/// 64 hex digits, display order
pub(crate) fn txid<'de, D: Deserializer<'de>>(hex: D) -> Result<TxId, D::Error> {
    txid_hex(&String::deserialize(hex)?)
}

fn txid_hex<E: serde::de::Error>(hex: &str) -> Result<TxId, E> {
    let invalid = || E::custom(format!("not a txid (64 hex digits): {hex:.64}"));
    TxId::from_hex(hex).ok_or_else(invalid)
}

/// Hex-string `result` decoded straight off the body: one SIMD pass into an exact-size buffer
///
/// - `deserialize_bytes`: serde_json hands the string's raw bytes (no UTF-8 pass)
/// - Non-hex byte = hex error
pub(crate) struct HexBytes(pub(crate) Vec<u8>);

impl<'de> Deserialize<'de> for HexBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
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
/// - Branch id unknown to this build = skipped, its entry unread
/// - Activations in protocol order (a JSON object carries none)
pub(crate) fn parse_blockchain_info(value: Value) -> Result<BlockchainInfo, ParseError> {
    #[derive(Deserialize)]
    struct Reply {
        #[serde(deserialize_with = "block_hash")]
        bestblockhash: BlockHash,
        blocks: u32,
        estimatedheight: u32,
        upgrades: BTreeMap<String, Value>,
    }
    #[derive(Deserialize)]
    struct Upgrade {
        activationheight: u32,
    }

    let reply = Reply::deserialize(value)?;
    let mut activations = Vec::new();
    for (key, upgrade) in reply.upgrades {
        let known = u32::from_str_radix(&key, 16).ok().and_then(|id| BranchId::try_from(id).ok());
        if let Some(branch) = known {
            let height = Upgrade::deserialize(upgrade)?.activationheight;
            activations.push((branch, BlockHeight::from(height)));
        }
    }
    activations.sort_by_key(|(branch, _)| {
        UPGRADES_IN_ORDER.iter().position(|upgrade| upgrade.branch_id() == *branch)
    });
    let tip = BlockRef { hash: reply.bestblockhash, height: BlockHeight::from(reply.blocks) };
    let estimated_height = BlockHeight::from(reply.estimatedheight);
    Ok(BlockchainInfo { tip, estimated_height, activations })
}

/// Entries accepted from one `getrawmempool` (ZIP-401 node holds ~8k)
///
/// - Trips only on a hostile node, before a million raw-transaction fetches follow
const MAX_MEMPOOL_LISTING_ENTRIES: usize = 1_000_000;

/// `getrawmempool true` (`{txid: {fee, ..}}`), refused on its count before any entry is decoded
pub(crate) fn parse_mempool_listing(value: Value) -> Result<Vec<MempoolListed>, ParseError> {
    #[derive(Deserialize)]
    struct Entry {
        fee: f64,
        size: u32,
    }

    let entries = BTreeMap::<String, Value>::deserialize(value)?;
    if entries.len() > MAX_MEMPOOL_LISTING_ENTRIES {
        let (len, max) = (entries.len(), MAX_MEMPOOL_LISTING_ENTRIES);
        return Err(ParseError::ListingTooLarge { len, max });
    }
    entries
        .into_iter()
        .map(|(txid, entry)| {
            let Entry { fee, size } = Entry::deserialize(entry)?;
            let txid = txid_hex::<serde_json::Error>(&txid)?;
            Ok(MempoolListed { txid, fee: zatoshis(fee)?, encoded_len: size })
        })
        .collect()
}

/// Decimal ZEC (zebra's `f64`) → zatoshis
///
/// - Exact: every in-supply count < 2^53, so `zats / 1e8` then `× 1e8` lands within 0.25 zat
fn zatoshis(zec: f64) -> Result<Zatoshis, ParseError> {
    let zats = (zec * 100_000_000.0).round();
    if !(0.0..=MAX_MONEY as f64).contains(&zats) {
        return Err(ParseError::Zec(zec));
    }
    Zatoshis::from_u64(zats as u64).map_err(|_| ParseError::Zec(zec))
}

/// `getrawtransaction <txid> 1`: `height` absent = mempool, `-1` = side chain, `>= 0` = best chain
///
/// - Any other negative refused (undefined, never folded into side chain)
pub(crate) fn parse_transaction(value: Value) -> Result<TransactionResponse, ParseError> {
    #[derive(Deserialize)]
    struct Reply {
        hex: HexBytes,
        height: Option<i64>,
    }

    let Reply { hex: HexBytes(bytes), height } = Reply::deserialize(value)?;
    let location = match height {
        None => TransactionLocation::Mempool,
        Some(-1) => TransactionLocation::NonBestChain,
        Some(height) => {
            let mined = u32::try_from(height).map_err(|_| ParseError::Height(height))?;
            TransactionLocation::BestChain(BlockHeight::from(mined))
        }
    };
    Ok(TransactionResponse { bytes, location })
}

/// `getinfo` + `getdeprecationinfo` (`None` = the release lacks the latter)
///
/// - `end_of_service` absent = not enforced on this network
pub(crate) fn parse_node_release(
    info: Value,
    deprecation: Option<Value>,
) -> Result<NodeRelease, ParseError> {
    #[derive(Deserialize)]
    struct Info {
        build: String,
        subversion: String,
    }
    #[derive(Deserialize)]
    struct Deprecation {
        end_of_service: Option<Halt>,
    }
    #[derive(Deserialize)]
    struct Halt {
        block_height: u32,
    }

    let Info { build, subversion } = Info::deserialize(info)?;
    let end_of_service = match deprecation.map(Deprecation::deserialize).transpose()? {
        None => EndOfService::Unknown,
        Some(Deprecation { end_of_service: None }) => EndOfService::NotEnforced,
        Some(Deprecation { end_of_service: Some(Halt { block_height }) }) => {
            EndOfService::At { height: BlockHeight::from(block_height) }
        }
    };
    Ok(NodeRelease { build, user_agent: subversion, end_of_service })
}

/// `getpeerinfo`
pub(crate) fn parse_peer_info(value: Value) -> Result<Vec<PeerInfo>, ParseError> {
    #[derive(Deserialize)]
    struct Peer {
        addr: String,
        inbound: bool,
    }

    let peers = Vec::<Peer>::deserialize(value)?;
    Ok(peers.into_iter().map(|Peer { addr, inbound }| PeerInfo { addr, inbound }).collect())
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
        let listed = |fee: Value| {
            parse_mempool_listing(json!({ ASYMMETRIC_HEX: { "size": 250, "fee": fee } }))
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
        let refused = |entry: Value| {
            let listing = parse_mempool_listing(json!({ ASYMMETRIC_HEX: entry }));
            listing.expect_err("refused").to_string()
        };
        assert!(refused(json!({ "size": 250 })).contains("missing field `fee`"));
        assert!(refused(json!({ "fee": 0.0001 })).contains("missing field `size`"));
        assert!(refused(json!({ "size": 1u64 << 32, "fee": 0 })).contains("u32"));

        let oversized: serde_json::Map<String, Value> = (0..=MAX_MEMPOOL_LISTING_ENTRIES)
            .map(|n| (format!("{n:064x}"), json!({ "fee": 0.0001 })))
            .collect();
        let oversized = parse_mempool_listing(Value::Object(oversized));
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
                "deadbeef": { "name": "NU9", "activationheight": "unread", "status": "pending" },
            },
            "consensus": { "chaintip": "c2d6d0b4", "nextblock": "deadbeef" },
        });
        let mut reversed_bytes = [0u8; 32];
        const_hex::decode_to_slice(ASYMMETRIC_HEX, &mut reversed_bytes).expect("fixture");
        reversed_bytes.reverse();
        let height = BlockHeight::from;

        let parsed = parse_blockchain_info(info.clone()).expect("well-formed");
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
        let no_height = parse_blockchain_info(no_height).expect_err("no NU5 height").to_string();
        assert!(no_height.contains("missing field `activationheight`"), "{no_height}");
        let mut no_tip = info;
        no_tip.as_object_mut().expect("object").remove("bestblockhash");
        let no_tip = parse_blockchain_info(no_tip).expect_err("no tip").to_string();
        assert!(no_tip.contains("missing field `bestblockhash`"), "{no_tip}");
    }

    /// - All three placements pinned (side-chain tx reported as unmined = reads as pending)
    /// - Undefined negative height refused
    #[test]
    fn transactions_place_themselves() {
        use TransactionLocation::{BestChain, Mempool, NonBestChain};
        let location = |value| parse_transaction(value).map(|tx| tx.location);
        let mined = location(json!({ "hex": "00", "height": 12345 })).expect("mined");
        let side = location(json!({ "hex": "00", "height": -1 })).expect("side chain");
        let mempool = location(json!({ "hex": "00" })).expect("mempool");
        let height = BlockHeight::from(12345);
        assert_eq!((mined, side, mempool), (BestChain(height), NonBestChain, Mempool));
        assert!(matches!(
            location(json!({ "hex": "00", "height": -7 })),
            Err(ParseError::Height(-7))
        ));
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
        let release = |deprecation: Option<Value>| {
            parse_node_release(info.clone(), deprecation).map(|release| release.end_of_service)
        };
        let mainnet = json!({ "end_of_service": { "block_height": 3_564_960, "estimated_time": 1_790_000_000 } });
        let halts = EndOfService::At { height: BlockHeight::from(3_564_960) };
        assert_eq!(release(Some(mainnet)).expect("mainnet"), halts);
        assert_eq!(release(Some(json!({}))).expect("testnet"), EndOfService::NotEnforced);
        assert_eq!(release(None).expect("zebrad 6.2"), EndOfService::Unknown);

        let read = parse_node_release(info.clone(), None).expect("getinfo");
        assert_eq!((read.build.as_str(), read.user_agent.as_str()), ("v6.4.2", "/Zebra:6.4.2/"));
        let no_height = json!({ "end_of_service": { "estimated_time": 1 } });
        let missing = release(Some(no_height)).expect_err("no halt height").to_string();
        assert!(missing.contains("missing field `block_height`"), "{missing}");
    }
}
