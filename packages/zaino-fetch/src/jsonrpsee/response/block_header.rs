//! Types associated with the `getblockheader` RPC request.

use serde::{Deserialize, Serialize};

use zebra_rpc::methods::opthex;

use crate::jsonrpsee::connector::{ResponseToError, RpcError};

/// Response to a `getblockheader` RPC request.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GetBlockHeader {
    /// The verbose variant of the response. Returned when `verbose` is set to `true`.
    Verbose(VerboseBlockHeader),

    /// The compact variant of the response. Returned when `verbose` is set to `false`.
    Compact(String),

    /// An unknown response shape.
    Unknown(serde_json::Value),
}

/// Error type for the `getblockheader` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum GetBlockHeaderError {
    /// Verbosity not valid
    #[error("Invalid verbosity: {0}")]
    InvalidVerbosity(i8),

    /// The requested block hash or height could not be found
    #[error("Block not found: {0}")]
    MissingBlock(String),
}

/// Verbose response to a `getblockheader` RPC request.
///
/// See the notes for the `get_block_header` method.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerboseBlockHeader {
    /// The hash of the requested block.
    #[serde(with = "hex")]
    pub hash: zebra_chain::block::Hash,

    /// The number of confirmations of this block in the best chain,
    /// or -1 if it is not in the best chain.
    pub confirmations: i64,

    /// The height of the requested block.
    pub height: u32,

    /// The version field of the requested block.
    pub version: u32,

    /// The merkle root of the requesteed block.
    #[serde(with = "hex", rename = "merkleroot")]
    pub merkle_root: zebra_chain::block::merkle::Root,

    /// The blockcommitments field of the requested block. Its interpretation changes
    /// depending on the network and height.
    ///
    /// This field is only present in Zebra. It was added [here](https://github.com/ZcashFoundation/zebra/pull/9217).
    #[serde(
        with = "opthex",
        rename = "blockcommitments",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub block_commitments: Option<[u8; 32]>,

    /// The root of the Sapling commitment tree after applying this block.
    #[serde(with = "opthex", rename = "finalsaplingroot")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_sapling_root: Option<[u8; 32]>,

    /// The block time of the requested block header in non-leap seconds since Jan 1 1970 GMT.
    pub time: i64,

    /// The nonce of the requested block header.
    pub nonce: String,

    /// The Equihash solution in the requested block header.
    pub solution: String,

    /// The difficulty threshold of the requested block header displayed in compact form.
    pub bits: String,

    /// Floating point number that represents the difficulty limit for this block as a multiple
    /// of the minimum difficulty for the network.
    pub difficulty: f64,

    /// Cumulative chain work for this block (hex).
    ///
    /// Present in zcashd, omitted by Zebra.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chainwork: Option<String>,

    /// The previous block hash of the requested block header.
    #[serde(
        rename = "previousblockhash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub previous_block_hash: Option<String>,

    /// The next block hash after the requested block header.
    #[serde(
        rename = "nextblockhash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub next_block_hash: Option<String>,
}

impl ResponseToError for GetBlockHeader {
    type RpcError = GetBlockHeaderError;
}

impl TryFrom<RpcError> for GetBlockHeaderError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        // If the block is not in Zebra's state, returns
        // [error code `-8`.](https://github.com/zcash/zcash/issues/5758)
        if value.code == -8 {
            Ok(Self::MissingBlock(value.message))
        } else {
            Err(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use hex::FromHex;
    use serde_json::{json, Value};
    use zebra_chain::block;

    /// Zcashd verbose response.
    fn zcashd_verbose_json() -> &'static str {
        r#"{
          "hash": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "confirmations": 10,
          "height": 123456,
          "version": 4,
          "merkleroot": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "finalsaplingroot": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "time": 1700000000,
          "nonce": "11nonce",
          "solution": "22solution",
          "bits": "1d00ffff",
          "difficulty": 123456.789,
          "chainwork": "0000000000000000000000000000000000000000000000000000000000001234",
          "previousblockhash": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "nextblockhash": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
        }"#
    }

    // Zebra verbose response
    fn zebra_verbose_json() -> &'static str {
        r#"{
          "hash": "00000000001b76b932f31289beccd3988d098ec3c8c6e4a0c7bcaf52e9bdead1",
          "confirmations": 3,
          "height": 42,
          "version": 5,
          "merkleroot": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "blockcommitments": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "finalsaplingroot": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "time": 1699999999,
          "nonce": "33nonce",
          "solution": "44solution",
          "bits": "1c654321",
          "difficulty": 7890.123,
          "previousblockhash": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
        }"#
    }

    #[test]
    fn deserialize_verbose_zcashd_includes_chainwork() {
        match serde_json::from_str::<VerboseBlockHeader>(zcashd_verbose_json()) {
            Ok(block_header) => {
                assert_eq!(
                    block_header.hash,
                    block::Hash::from_str(
                        "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
                    )
                    .unwrap()
                );
                assert_eq!(block_header.confirmations, 10);
                assert_eq!(block_header.height, 123_456);
                assert_eq!(block_header.version, 4);
                assert_eq!(
                    block_header.merkle_root,
                    block::merkle::Root::from_hex(
                        "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
                    )
                    .unwrap()
                );
                assert_eq!(
                    block_header.final_sapling_root.unwrap(),
                    <[u8; 32]>::from_hex(
                        "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
                    )
                    .unwrap()
                );
                assert_eq!(block_header.time, 1_700_000_000);
                assert_eq!(block_header.nonce, "11nonce");
                assert_eq!(block_header.solution, "22solution");
                assert_eq!(block_header.bits, "1d00ffff");
                assert!((block_header.difficulty - 123_456.789).abs() < f64::EPSILON);

                assert_eq!(
                    block_header.chainwork.as_deref(),
                    Some("0000000000000000000000000000000000000000000000000000000000001234")
                );

                assert_eq!(
                    block_header.previous_block_hash.as_deref(),
                    Some("000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f")
                );
                assert_eq!(
                    block_header.next_block_hash.as_deref(),
                    Some("000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f")
                );
            }
            Err(e) => {
                panic!(
                    "VerboseBlockHeader failed at {}:{} — {}",
                    e.line(),
                    e.column(),
                    e
                );
            }
        }
    }

    #[test]
    fn deserialize_verbose_zebra_includes_blockcommitments_and_omits_chainwork() {
        match serde_json::from_str::<VerboseBlockHeader>(zebra_verbose_json()) {
            Ok(block_header) => {
                assert_eq!(
                    block_header.hash,
                    block::Hash::from_str(
                        "00000000001b76b932f31289beccd3988d098ec3c8c6e4a0c7bcaf52e9bdead1"
                    )
                    .unwrap()
                );
                assert_eq!(block_header.confirmations, 3);
                assert_eq!(block_header.height, 42);
                assert_eq!(block_header.version, 5);
                assert_eq!(
                    block_header.merkle_root,
                    block::merkle::Root::from_hex(
                        "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
                    )
                    .unwrap()
                );

                assert_eq!(
                    block_header.block_commitments.unwrap(),
                    <[u8; 32]>::from_hex(
                        "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
                    )
                    .unwrap()
                );

                assert_eq!(
                    block_header.final_sapling_root.unwrap(),
                    <[u8; 32]>::from_hex(
                        "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
                    )
                    .unwrap()
                );

                assert_eq!(block_header.time, 1_699_999_999);
                assert_eq!(block_header.nonce, "33nonce");
                assert_eq!(block_header.solution, "44solution");
                assert_eq!(block_header.bits, "1c654321");
                assert!((block_header.difficulty - 7890.123).abs() < f64::EPSILON);

                assert!(block_header.chainwork.is_none());

                // Zebra always sets previous
                assert_eq!(
                    block_header.previous_block_hash.as_deref(),
                    Some("000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f")
                );
                assert!(block_header.next_block_hash.is_none());
            }
            Err(e) => {
                panic!(
                    "VerboseBlockHeader failed at {}:{} — {}",
                    e.line(),
                    e.column(),
                    e
                );
            }
        }
    }

    #[test]
    fn compact_header_is_hex_string() {
        let s = r#""040102deadbeef""#;
        let block_header: GetBlockHeader = serde_json::from_str(s).unwrap();
        match block_header.clone() {
            GetBlockHeader::Compact(hex) => assert_eq!(hex, "040102deadbeef"),
            _ => panic!("expected Compact variant"),
        }

        // Roundtrip
        let out = serde_json::to_string(&block_header).unwrap();
        assert_eq!(out, s);
    }

    #[test]
    fn unknown_shape_falls_back_to_unknown_variant() {
        let weird = r#"{ "weird": 1, "unexpected": ["a","b","c"] }"#;
        let block_header: GetBlockHeader = serde_json::from_str(weird).unwrap();
        match block_header {
            GetBlockHeader::Unknown(v) => {
                assert_eq!(v["weird"], json!(1));
                assert_eq!(v["unexpected"], json!(["a", "b", "c"]));
            }
            _ => panic!("expected Unknown variant"),
        }
    }

    #[test]
    fn zebra_roundtrip_does_not_inject_chainwork_field() {
        let block_header: GetBlockHeader = serde_json::from_str(zebra_verbose_json()).unwrap();
        let header_value: Value = serde_json::to_value(&block_header).unwrap();

        let header_object = header_value
            .as_object()
            .expect("verbose should serialize to object");
        assert!(!header_object.contains_key("chainwork"));

        assert_eq!(
            header_object.get("blockcommitments"),
            Some(&json!(
                "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f"
            ))
        );
    }

    #[test]
    fn zcashd_roundtrip_preserves_chainwork() {
        let block_header: GetBlockHeader = serde_json::from_str(zcashd_verbose_json()).unwrap();
        let header_value: Value = serde_json::to_value(&block_header).unwrap();
        let header_object = header_value.as_object().unwrap();

        assert_eq!(
            header_object.get("chainwork"),
            Some(&json!(
                "0000000000000000000000000000000000000000000000000000000000001234"
            ))
        );
    }

    #[test]
    fn previous_and_next_optional_edges() {
        // Simulate genesis
        let genesis_like = r#"{
          "hash": "00000000001b76b932f31289beccd3988d098ec3c8c6e4a0c7bcaf52e9bdead1",
          "confirmations": 1,
          "height": 0,
          "version": 4,
          "merkleroot": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "finalsaplingroot": "000000000053d2771290ff1b57181bd067ae0e55a367ba8ddee2d961ea27a14f",
          "time": 1477641369,
          "nonce": "nonce",
          "solution": "solution",
          "bits": "1d00ffff",
          "difficulty": 1.0
        }"#;

        match serde_json::from_str::<VerboseBlockHeader>(genesis_like) {
            Ok(block_header) => {
                assert!(block_header.previous_block_hash.is_none());
                assert!(block_header.next_block_hash.is_none());
            }
            Err(e) => {
                panic!(
                    "VerboseBlockHeader failed at {}:{} — {}",
                    e.line(),
                    e.column(),
                    e
                );
            }
        }
    }
}
