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
    Ok(Zatoshis::from_u64(zats as u64).expect("zats range-checked to 0..=MAX_MONEY"))
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

    fn reversed_asymmetric() -> [u8; 32] {
        let mut bytes = [0u8; 32];
        const_hex::decode_to_slice(ASYMMETRIC_HEX, &mut bytes).expect("fixture");
        bytes.reverse();
        bytes
    }

    /// Zebra's verbose listing: key reversed → txid, `fee` (lossy `f64` ZEC) → exact zatoshis,
    /// `size` = encoded length
    #[test]
    fn a_listed_fee_reads_back_as_exact_zatoshis() {
        #[rustfmt::skip]
        let cases = [
            ("ZIP 317 minimum",    json!(0.0001),        10_000),
            ("1.5 × minimum",      json!(0.00015),       15_000),
            ("1 zat",              json!(1e-8),          1),
            ("sub-zat f64 noise",  json!(0.1 + 0.2),     30_000_000),
            ("whole supply",       json!(21_000_000.0),  2_100_000_000_000_000),
            ("integer zero",       json!(0),             0),
        ];
        for (case, fee, zats) in cases {
            let listing = json!({ ASYMMETRIC_HEX: { "size": 250, "fee": fee } });

            let listed = parse_mempool_listing(listing).map_err(|e| e.to_string());

            let fee = Zatoshis::from_u64(zats).expect("in supply");
            let txid = TxId::from_bytes(reversed_asymmetric());
            let expected = MempoolListed { txid, fee, encoded_len: 250 };
            assert_eq!(listed, Ok(vec![expected]), "{case}");
        }
    }

    #[test]
    fn a_fee_outside_the_money_supply_is_refused_naming_it() {
        for (case, fee) in [("negative", -0.0001), ("supply + 1 zat", 21_000_000.00000001)] {
            let listing = json!({ ASYMMETRIC_HEX: { "size": 250, "fee": fee } });

            let refused = parse_mempool_listing(listing);

            let named = matches!(refused, Err(ParseError::Zec(zec)) if zec == fee);
            assert!(named, "{case} fee {fee}: {refused:?}");
        }
    }

    #[test]
    fn a_malformed_listing_entry_is_refused_naming_the_field() {
        #[rustfmt::skip]
        let cases = [
            ("fee as a string", json!({ "size": 250, "fee": "0.0001" }), "invalid type: string"),
            ("no fee",          json!({ "size": 250 }),                  "missing field `fee`"),
            ("no size",         json!({ "fee": 0.0001 }),                "missing field `size`"),
            ("size past u32",   json!({ "size": 1u64 << 32, "fee": 0 }), "u32"),
        ];
        for (case, entry, named) in cases {
            let refused = parse_mempool_listing(json!({ ASYMMETRIC_HEX: entry }));

            let message = refused.map_err(|e| e.to_string());
            assert!(matches!(&message, Err(m) if m.contains(named)), "{case}: {message:?}");
        }
    }

    /// Count checked before any entry decodes: `MAX_MEMPOOL_LISTING_ENTRIES` read, one more refused
    #[test]
    fn a_listing_holds_at_most_max_mempool_listing_entries() {
        let listing = |count: usize| {
            let entries = (0..count).map(|n| (format!("{n:064x}"), json!({ "size": 1, "fee": 0 })));
            parse_mempool_listing(Value::Object(entries.collect())).map(|listed| listed.len())
        };
        let max = MAX_MEMPOOL_LISTING_ENTRIES;

        let (at_cap, over) = (listing(max), listing(max + 1));

        assert!(matches!(at_cap, Ok(read) if read == max), "{max} entries: {at_cap:?}");
        assert!(
            matches!(over, Err(ParseError::ListingTooLarge { len, max: cap }) if len == max + 1 && cap == max),
            "{} entries: {over:?}",
            max + 1
        );
    }

    /// Never fail the poll on what it does not read: lossy pool floats, an upgrade this build
    /// does not know (a validator ahead of the pin), names, statuses
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

        let parsed = parse_blockchain_info(info).expect("well-formed");

        let height = BlockHeight::from;
        let tip = BlockRef { hash: BlockHash(reversed_asymmetric()), height: height(3_451_543) };
        assert_eq!(parsed.tip, tip, "tip: bestblockhash reversed, blocks");
        assert_eq!(parsed.estimated_height, height(3_451_544), "estimatedheight");
        let known = [(BranchId::Sapling, height(419_200)), (BranchId::Nu5, height(1_687_104))];
        assert_eq!(
            parsed.activations, known,
            "protocol order, NU9 (unknown to this build) skipped"
        );
    }

    #[test]
    fn blockchain_info_without_a_tip_or_a_known_upgrades_height_is_refused() {
        let no_height = json!({
            "blocks": 1, "estimatedheight": 1, "bestblockhash": ASYMMETRIC_HEX,
            "upgrades": { "c2d6d0b4": { "name": "NU5", "status": "active" } },
        });
        let no_tip = json!({ "blocks": 1, "estimatedheight": 1, "upgrades": {} });

        for (case, info, named) in [
            ("NU5 without activationheight", no_height, "missing field `activationheight`"),
            ("no bestblockhash", no_tip, "missing field `bestblockhash`"),
        ] {
            let refused =
                parse_blockchain_info(info).map(|info| info.tip).map_err(|e| e.to_string());
            assert!(matches!(&refused, Err(m) if m.contains(named)), "{case}: {refused:?}");
        }
    }

    /// Side-chain tx (`-1`) = reads as off the best chain, any other negative height refused
    #[test]
    fn transactions_place_themselves() {
        use TransactionLocation::{BestChain, Mempool, NonBestChain};
        let refused = Err("height -7: neither -1 nor a block height".to_owned());
        #[rustfmt::skip]
        let cases = [
            ("height 12345", json!({ "hex": "00", "height": 12345 }), Ok(BestChain(BlockHeight::from(12345)))),
            ("height -1",    json!({ "hex": "00", "height": -1 }),    Ok(NonBestChain)),
            ("no height",    json!({ "hex": "00" }),                  Ok(Mempool)),
            ("height -7",    json!({ "hex": "00", "height": -7 }),    refused),
        ];
        for (case, reply, expected) in cases {
            let location =
                parse_transaction(reply).map(|tx| tx.location).map_err(|e| e.to_string());
            assert_eq!(location, expected, "{case}");
        }
    }

    /// zebrad 6.4.2's shapes: mainnet → halt height, other networks → omitted, a release before
    /// `getdeprecationinfo` → unknown (never read as "no halt")
    #[test]
    fn a_release_reads_its_build_and_where_it_halts() {
        let getinfo = json!({
            "version": 6040250, "build": "v6.4.2", "subversion": "/Zebra:6.4.2/",
            "protocolversion": 170140, "blocks": 3_400_000, "connections": 8,
        });
        let halt = json!({ "end_of_service": { "block_height": 3_564_960, "estimated_time": 1_790_000_000 } });
        #[rustfmt::skip]
        let cases = [
            ("mainnet",    Some(halt),     EndOfService::At { height: BlockHeight::from(3_564_960) }),
            ("testnet",    Some(json!({})), EndOfService::NotEnforced),
            ("zebrad 6.2", None,           EndOfService::Unknown),
        ];
        for (case, deprecation, expected) in cases {
            let read = parse_node_release(getinfo.clone(), deprecation).map_err(|e| e.to_string());

            let read = read.unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(read.end_of_service, expected, "{case}: end of service");
            assert_eq!(read.build, "v6.4.2", "{case}: build");
            assert_eq!(read.user_agent, "/Zebra:6.4.2/", "{case}: user agent (subversion)");
        }
    }

    #[test]
    fn a_deprecation_without_its_halt_height_is_refused() {
        let getinfo = json!({ "build": "v6.4.2", "subversion": "/Zebra:6.4.2/" });
        let no_height = json!({ "end_of_service": { "estimated_time": 1 } });

        let refused = parse_node_release(getinfo, Some(no_height)).map_err(|e| e.to_string());

        let refused = refused.map(|release| release.end_of_service);
        let named = matches!(&refused, Err(m) if m.contains("missing field `block_height`"));
        assert!(named, "{refused:?}");
    }
}
