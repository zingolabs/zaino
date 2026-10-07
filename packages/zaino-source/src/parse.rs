//! JSON-RPC `result` → zaino-primitives types, one parser per method
//!
//! - Strict where Zaino acts on a value (an unknown upgrade status fails the parse: a short
//!   schedule would put Zaino on other consensus rules than its validator)
//! - Absent / `null` optional field = `None`; present but malformed = error ("inactive" and
//!   "garbled" are different facts)

use zaino_primitives::types::{
    BlockHash, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds, EndOfService, Height,
    HeightOverflow, NetworkUpgradeInfo, NetworkUpgradeStatus, NodeRelease, ParseHashError,
    PeerInfo, TransactionId, TransactionLocation, Zatoshis,
};

use zcash_protocol::consensus::BranchId;

use crate::{MempoolListed, TransactionResponse};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    #[error("hex decode: {0}")]
    Hex(String),
    #[error("hash: {0}")]
    Hash(#[from] ParseHashError),
    #[error("expected {expected}, got {got}")]
    UnexpectedType { expected: &'static str, got: String },
    #[error("value {0} overflows target type")]
    Overflow(u64),
    #[error("invalid height: {0}")]
    Height(#[from] HeightOverflow),
    #[error("missing field `{0}`")]
    MissingField(&'static str),
    #[error("unknown network upgrade status `{0}`")]
    UpgradeStatus(String),
    #[error("upgrade schedule has no Sapling entry")]
    NoSapling,
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

fn as_height(value: &serde_json::Value) -> Result<Height, ParseError> {
    Ok(Height::try_from(as_u64(value)?)?)
}

fn hex(value: &serde_json::Value) -> Result<Vec<u8>, ParseError> {
    hex::decode(as_str(value)?).map_err(|e| ParseError::Hex(e.to_string()))
}

pub(crate) fn as_txid(value: &serde_json::Value) -> Result<TransactionId, ParseError> {
    Ok(as_str(value)?.parse()?)
}

pub(crate) fn as_block_hash(value: &serde_json::Value) -> Result<BlockHash, ParseError> {
    Ok(as_str(value)?.parse()?)
}

/// Hex-string `result` decoded straight off the body (borrowed `&str`, one const-hex pass)
pub(crate) struct HexBytes(pub(crate) Vec<u8>);

impl<'de> serde::Deserialize<'de> for HexBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = HexBytes;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a hex string")
            }

            fn visit_str<E: serde::de::Error>(self, hex: &str) -> Result<HexBytes, E> {
                hex::decode(hex).map(HexBytes).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

/// `getblockchaininfo`: the tip + the upgrade schedule (a consensus input: strict); every other
/// field ignored (never read, so never a reason to fail)
pub(crate) fn parse_blockchain_info(
    value: &serde_json::Value,
) -> Result<BlockchainInfo, ParseError> {
    let consensus = field(value, "consensus")?;
    let upgrades = parse_upgrades(field(value, "upgrades")?)?;
    let sapling = ConsensusBranchId::new(u32::from(BranchId::Sapling));
    let sapling_activation = upgrades
        .iter()
        .find(|upgrade| upgrade.branch_id == sapling)
        .ok_or(ParseError::NoSapling)?
        .activation_height;
    Ok(BlockchainInfo {
        blocks: as_height(field(value, "blocks")?)?,
        estimated_height: as_height(field(value, "estimatedheight")?)?,
        best_block_hash: as_block_hash(field(value, "bestblockhash")?)?,
        sapling_activation,
        upgrades,
        consensus: ConsensusBranchIds {
            chain_tip: parse_branch_id(as_str(field(consensus, "chaintip")?)?)?,
            next_block: parse_branch_id(as_str(field(consensus, "nextblock")?)?)?,
        },
    })
}

fn parse_branch_id(s: &str) -> Result<ConsensusBranchId, ParseError> {
    u32::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16)
        .map(ConsensusBranchId::new)
        .map_err(|e| ParseError::Hex(format!("consensus branch id `{s}`: {e}")))
}

/// Keyed by consensus branch id (the upgrade's identity; the name is a label)
fn parse_upgrades(value: &serde_json::Value) -> Result<Vec<NetworkUpgradeInfo>, ParseError> {
    let map = value.as_object().ok_or_else(|| ParseError::unexpected("object", value))?;

    map.iter()
        .map(|(branch_id, info)| {
            Ok(NetworkUpgradeInfo {
                branch_id: parse_branch_id(branch_id)?,
                name: as_str(field(info, "name")?)?.to_owned(),
                activation_height: as_height(field(info, "activationheight")?)?,
                status: match as_str(field(info, "status")?)? {
                    "active" => NetworkUpgradeStatus::Active,
                    "pending" => NetworkUpgradeStatus::Pending,
                    other => return Err(ParseError::UpgradeStatus(other.to_owned())),
                },
            })
        })
        .collect()
}

/// Entries accepted from one `getrawmempool` (a ZIP-401 node holds ~8k; this only trips on a
/// hostile node, before a million raw-transaction fetches follow)
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
                txid: txid.parse()?,
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
    if !(0.0..=Zatoshis::MAX.as_u64() as f64).contains(&zats) {
        return Err(ParseError::Zec(zec));
    }
    Zatoshis::new(zats as u64).map_err(|_| ParseError::Zec(zec))
}

/// `getrawtransaction <txid> 0`: a bare hex string
pub(crate) fn parse_raw_transaction(value: &serde_json::Value) -> Result<Vec<u8>, ParseError> {
    hex(value)
}

/// `getrawtransaction <txid> 1`: `height` absent = mempool, `-1` = side chain, `>= 0` = best
/// chain; any other negative refused (undefined, never folded into side chain)
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
            Some(at) => EndOfService::At {
                height: as_height(field(at, "block_height")?)?,
                estimated_unix: as_i64(field(at, "estimated_time")?)?,
            },
        },
    };
    let protocol = as_u64(field(info, "protocolversion")?)?;
    Ok(NodeRelease {
        build: as_str(field(info, "build")?)?.to_owned(),
        user_agent: as_str(field(info, "subversion")?)?.to_owned(),
        protocol_version: u32::try_from(protocol).map_err(|_| ParseError::Overflow(protocol))?,
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

    /// Zebra's verbose listing (`fee` = lossy `f64` ZEC): each key reversed into a txid, each fee
    /// back to its exact zatoshis (ZIP-317 minimum, sub-zat float noise, the whole supply), `size`
    /// as the encoded length; a negative or past-supply fee, a missing fee or size, a size past
    /// `u32` and an over-cap listing refused
    #[test]
    fn mempool_listing_reads_each_txid_and_its_exact_fee() {
        let mut reversed_bytes = [0u8; 32];
        hex::decode_to_slice(ASYMMETRIC_HEX, &mut reversed_bytes).expect("fixture");
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
            let txid = TransactionId::from(reversed_bytes);
            let fee_zats = Zatoshis::new(zats).expect("in supply");
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

    /// The fields read reach the domain (hash reversed, schedule keyed by branch id, Sapling's
    /// activation lifted out); fields never read (lossy pool floats: the old mainnet-boot crash)
    /// cannot fail the parse; an unknown status, a missing name or no Sapling entry fails it
    #[test]
    fn blockchain_info_reads_the_tip_and_schedule_and_ignores_the_rest() {
        let info = json!({
            "chain": "main",
            "blocks": 3_451_543,
            "estimatedheight": 3_451_544,
            "bestblockhash": ASYMMETRIC_HEX,
            "chainSupply": { "chainValue": "not even a number" },
            "valuePools": [{ "id": "sapling", "chainValue": 529_544.04149098 }],
            "upgrades": {
                "76b809bb": { "name": "Sapling", "activationheight": 419_200, "status": "active" },
                "c2d6d0b4": { "name": "NU5", "activationheight": 1_687_104, "status": "active" },
            },
            "consensus": { "chaintip": "c2d6d0b4", "nextblock": "c2d6d0b4" },
        });
        let mut reversed_bytes = [0u8; 32];
        hex::decode_to_slice(ASYMMETRIC_HEX, &mut reversed_bytes).expect("fixture");
        reversed_bytes.reverse();
        let height = |h: u32| Height::try_from(h).expect("h");
        let nu5 = ConsensusBranchId::new(0xc2d6_d0b4);

        let expected = BlockchainInfo {
            blocks: height(3_451_543),
            estimated_height: height(3_451_544),
            best_block_hash: BlockHash::from(reversed_bytes),
            sapling_activation: height(419_200),
            upgrades: vec![
                NetworkUpgradeInfo {
                    branch_id: ConsensusBranchId::new(0x76b8_09bb),
                    name: "Sapling".to_owned(),
                    activation_height: height(419_200),
                    status: NetworkUpgradeStatus::Active,
                },
                NetworkUpgradeInfo {
                    branch_id: nu5,
                    name: "NU5".to_owned(),
                    activation_height: height(1_687_104),
                    status: NetworkUpgradeStatus::Active,
                },
            ],
            consensus: ConsensusBranchIds { chain_tip: nu5, next_block: nu5 },
        };
        assert_eq!(parse_blockchain_info(&info).expect("well-formed"), expected);

        let mut unknown = info.clone();
        unknown["upgrades"]["c2d6d0b4"]["status"] = json!("disabled");
        let unknown = parse_blockchain_info(&unknown);
        assert!(matches!(unknown, Err(ParseError::UpgradeStatus(status)) if status == "disabled"));
        let mut unnamed = info.clone();
        unnamed["upgrades"]["c2d6d0b4"].as_object_mut().expect("object").remove("name");
        assert!(matches!(parse_blockchain_info(&unnamed), Err(ParseError::MissingField("name"))));
        let mut no_sapling = info;
        no_sapling["upgrades"].as_object_mut().expect("object").remove("76b809bb");
        assert!(matches!(parse_blockchain_info(&no_sapling), Err(ParseError::NoSapling)));
    }

    /// All three placements pinned (a side-chain tx reported as unmined reads as still
    /// pending); an undefined negative height is refused; verbosity 0 is a bare string
    #[test]
    fn transactions_place_themselves_and_raw_bytes_are_a_bare_string() {
        use TransactionLocation::{BestChain, Mempool, NonBestChain};
        let location = |value| parse_transaction(&value).map(|tx| tx.location);
        let mined = location(json!({ "hex": "00", "height": 12345 })).expect("mined");
        let side = location(json!({ "hex": "00", "height": -1 })).expect("side chain");
        let mempool = location(json!({ "hex": "00" })).expect("mempool");
        let height = Height::try_from(12345u32).expect("h");
        assert_eq!((mined, side, mempool), (BestChain(height), NonBestChain, Mempool));
        assert!(location(json!({ "hex": "00", "height": -7 })).is_err());

        let raw = parse_raw_transaction(&json!("deadbeef")).expect("hex");
        assert_eq!(raw, [0xde, 0xad, 0xbe, 0xef]);
        let object = parse_raw_transaction(&json!({ "hex": "deadbeef" }));
        assert!(matches!(object, Err(ParseError::UnexpectedType { .. })));
    }

    /// zebrad 6.4.2's shapes: mainnet names the halt height, other networks omit it, a release
    /// before `getdeprecationinfo` says nothing (never read as "no halt")
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
        let halts = EndOfService::At {
            height: Height::try_from(3_564_960u32).expect("in range"),
            estimated_unix: 1_790_000_000,
        };
        assert_eq!(release(Some(mainnet)).expect("mainnet"), halts);
        assert_eq!(release(Some(json!({}))).expect("testnet"), EndOfService::NotEnforced);
        assert_eq!(release(None).expect("zebrad 6.2"), EndOfService::Unknown);

        let read = parse_node_release(&info, None).expect("getinfo");
        assert_eq!(
            (read.build.as_str(), read.user_agent.as_str(), read.protocol_version),
            ("v6.4.2", "/Zebra:6.4.2/", 170_140)
        );
        let no_height = json!({ "end_of_service": { "estimated_time": 1 } });
        let missing = parse_node_release(&info, Some(&no_height));
        assert!(matches!(missing, Err(ParseError::MissingField("block_height"))));
    }
}
