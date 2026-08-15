//! Types associated with the `getaddressdeltas` RPC request.

use serde::{Deserialize, Serialize};

/// Request parameters for the `getaddressdeltas` RPC method.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum GetAddressDeltasParams {
    /// Extends the basic address/height range with chaininfo and multiple address support.
    Filtered {
        /// List of base58check encoded addresses
        addresses: Vec<String>,

        /// Start block height (inclusive)
        #[serde(default)]
        start: u32,

        /// End block height (inclusive)
        #[serde(default)]
        end: u32,

        /// Whether to include chain info in response (defaults to false)
        #[serde(default, rename = "chainInfo")]
        chain_info: bool,
    },

    /// Get deltas for a single transparent address
    Address(String),
}

impl GetAddressDeltasParams {
    /// Creates a new [`GetAddressDeltasParams::Filtered`] instance.
    pub fn new_filtered(addresses: Vec<String>, start: u32, end: u32, chain_info: bool) -> Self {
        GetAddressDeltasParams::Filtered {
            addresses,
            start,
            end,
            chain_info,
        }
    }

    /// Creates a new [`GetAddressDeltasParams::Address`] instance.
    pub fn new_address(addr: impl Into<String>) -> Self {
        GetAddressDeltasParams::Address(addr.into())
    }

    /// Reads the client's request into the domain vocabulary.
    ///
    /// Infallible, unlike the other request conversions in this module. The two
    /// things that could be rejected here are not rejected by this interface:
    /// the height range is open-ended by design — zcashd reads `0` in either
    /// position as "unbounded" — and is resolved against the tip by the
    /// answering adapter, which is the only layer that knows the tip; and
    /// [`TransparentAddress`](zaino_primitives::types::TransparentAddress) is
    /// an opaque string, so there is nothing to parse.
    ///
    /// Validating the addresses here is now possible — `zaino-address` can
    /// classify them — but it would start rejecting requests this method
    /// currently answers with an empty list, which is a served-behaviour change
    /// and does not belong in a rewire.
    pub fn into_domain(self) -> zaino_primitives::types::rpc::AddressDeltasRequest {
        use zaino_primitives::types::{rpc::AddressDeltasRequest, TransparentAddress};

        match self {
            Self::Address(address) => {
                AddressDeltasRequest::Address(TransparentAddress::new(address))
            }
            Self::Filtered {
                addresses,
                start,
                end,
                chain_info,
            } => AddressDeltasRequest::Filtered {
                addresses: addresses.into_iter().map(TransparentAddress::new).collect(),
                start,
                end,
                chain_info,
            },
        }
    }
}

/// Response to a `getaddressdeltas` RPC request.
///
/// This enum supports both simple array responses and extended responses with chain info.
/// The format depends on the `chaininfo` parameter in the request.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum GetAddressDeltasResponse {
    /// Simple array format (chaininfo = false or not specified)
    /// Returns: [AddressDelta, AddressDelta, ...]
    Simple(Vec<AddressDelta>),
    /// Extended format with chain info (chaininfo = true)
    /// Returns: {"deltas": [...], "start": {...}, "end": {...}}
    WithChainInfo {
        /// The address deltas
        deltas: Vec<AddressDelta>,

        /// Information about the start block
        start: BlockInfo,

        /// Information about the end block
        end: BlockInfo,
    },
}

impl GetAddressDeltasResponse {
    /// Renders the domain answer as the served JSON shape.
    ///
    /// Which variant is emitted follows the domain value, which in turn follows
    /// the request — a `chainInfo` request with no deltas still answers the
    /// extended shape with an empty list.
    pub fn from_domain(deltas: zaino_primitives::types::rpc::AddressDeltas) -> Self {
        use zaino_primitives::types::rpc::AddressDeltas;

        match deltas {
            AddressDeltas::Simple(deltas) => Self::Simple(AddressDelta::vec_from_domain(deltas)),
            AddressDeltas::WithChainInfo { deltas, start, end } => Self::WithChainInfo {
                deltas: AddressDelta::vec_from_domain(deltas),
                start: BlockInfo::from_domain(start),
                end: BlockInfo::from_domain(end),
            },
        }
    }
}

/// Represents a change in the balance of a transparent address.
#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AddressDelta {
    /// The difference in zatoshis (or satoshis equivalent in Zcash)
    satoshis: i64,

    /// The related transaction ID in hex string format
    txid: String,

    /// The related input or output index
    pub index: u32,

    /// The block height where the change occurred
    pub height: u32,

    /// The base58check encoded address
    address: String,

    #[serde(rename = "blockindex", skip_serializing_if = "Option::is_none")]
    /// Zero-based position of the transaction within its containing block.
    pub block_index: Option<u32>,
}

impl AddressDelta {
    /// Creates a transparent address delta from already-indexed address-history data.
    ///
    /// This is used by backing stores and test sources that already know the
    /// address, value, transaction location, and input/output index. It avoids
    /// forcing those implementations to build partial `TransactionObject`
    /// values solely to immediately decompose them again.
    pub fn new(
        satoshis: i64,
        txid: String,
        index: u32,
        height: u32,
        address: String,
        block_index: Option<u32>,
    ) -> Self {
        Self {
            satoshis,
            txid,
            index,
            height,
            address,
            block_index,
        }
    }

    /// Renders a run of domain deltas as the served JSON shape.
    ///
    /// Ordering is the source's — zcashd's documented `(height, blockindex,
    /// index)` — and is not re-sorted here.
    fn vec_from_domain(deltas: Vec<zaino_primitives::types::AddressDelta>) -> Vec<Self> {
        deltas
            .into_iter()
            .map(|delta| Self {
                satoshis: delta.satoshis.into(),
                txid: super::display_hex(delta.txid.into()),
                index: delta.index,
                height: delta.height.into(),
                address: delta.address.into(),
                block_index: delta.block_index,
            })
            .collect()
    }
}

/// Block information for `getaddressdeltas` responses with `chaininfo = true`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct BlockInfo {
    /// The block hash in hex-encoded display order
    pub hash: String,
    /// The block height
    pub height: u32,
}

impl BlockInfo {
    /// Creates a new BlockInfo from a hash in hex-encoded display order and height.
    pub fn new(hash: String, height: u32) -> Self {
        Self { hash, height }
    }

    /// Renders a domain block reference as the served JSON shape.
    fn from_domain(block: zaino_primitives::types::BlockRef) -> Self {
        Self {
            hash: super::display_hex(block.hash.into()),
            height: block.height.into(),
        }
    }
}

#[cfg(test)]
mod domain_tests {
    use super::*;
    use zaino_primitives::types::{self as domain, Height, SignedZatoshis, TransparentAddress};

    /// Asymmetric under reversal, so a missing or doubled byte-reversal shows up.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    fn delta() -> domain::AddressDelta {
        domain::AddressDelta {
            satoshis: SignedZatoshis::new(-1_000),
            txid: domain::TransactionId::from(ASYMMETRIC),
            index: 2,
            height: Height::try_from(99u32).unwrap(),
            address: TransparentAddress::new("t1address".to_string()),
            block_index: Some(4),
        }
    }

    #[test]
    fn simple_response_is_a_bare_array_with_display_order_txids() {
        let json = serde_json::to_value(GetAddressDeltasResponse::from_domain(
            domain::rpc::AddressDeltas::Simple(vec![delta()]),
        ))
        .unwrap();

        let mut display_order = ASYMMETRIC;
        display_order.reverse();

        assert!(
            json.is_array(),
            "the simple form is not wrapped in an object"
        );
        assert_eq!(json[0]["txid"], hex::encode(display_order));
        assert_eq!(json[0]["satoshis"], -1_000i64);
        assert_eq!(json[0]["index"], 2);
        assert_eq!(json[0]["height"], 99);
        assert_eq!(json[0]["address"], "t1address");
        assert_eq!(json[0]["blockindex"], 4);
    }

    /// The chaininfo form's endpoints name blocks by hash, in display order —
    /// the same encoding as a delta's txid, from a different domain type.
    #[test]
    fn chain_info_response_names_its_endpoints() {
        let block = |height: u32| domain::BlockRef {
            hash: domain::BlockHash::from(ASYMMETRIC),
            height: Height::try_from(height).unwrap(),
        };

        let json = serde_json::to_value(GetAddressDeltasResponse::from_domain(
            domain::rpc::AddressDeltas::WithChainInfo {
                deltas: vec![delta()],
                start: block(1),
                end: block(99),
            },
        ))
        .unwrap();

        let mut display_order = ASYMMETRIC;
        display_order.reverse();

        assert_eq!(json["start"]["height"], 1);
        assert_eq!(json["start"]["hash"], hex::encode(display_order));
        assert_eq!(json["end"]["height"], 99);
        assert_eq!(json["deltas"][0]["satoshis"], -1_000i64);
    }

    /// A `chainInfo` request with no deltas still answers the extended shape:
    /// the variant follows the request, not what the data turned out to be.
    #[test]
    fn an_empty_chain_info_answer_keeps_its_shape() {
        let block = domain::BlockRef {
            hash: domain::BlockHash::from(ASYMMETRIC),
            height: Height::try_from(1u32).unwrap(),
        };

        let json = serde_json::to_value(GetAddressDeltasResponse::from_domain(
            domain::rpc::AddressDeltas::WithChainInfo {
                deltas: Vec::new(),
                start: block,
                end: block,
            },
        ))
        .unwrap();

        assert_eq!(json["deltas"], serde_json::json!([]));
        assert!(json["start"].is_object());
    }

    /// The request's height range reaches the domain unresolved: `0` still
    /// means "unbounded" here, because only the answering adapter knows the tip.
    #[test]
    fn request_range_is_carried_through_unclamped() {
        let request = GetAddressDeltasParams::new_filtered(
            vec!["t1a".to_string(), "t1b".to_string()],
            0,
            0,
            true,
        )
        .into_domain();

        let domain::rpc::AddressDeltasRequest::Filtered {
            addresses,
            start,
            end,
            chain_info,
        } = request
        else {
            panic!("a filtered request must stay filtered");
        };
        assert_eq!(addresses.len(), 2);
        assert_eq!((start, end, chain_info), (0, 0, true));
    }

    /// The single-address form has no range at all, and must not acquire one.
    #[test]
    fn single_address_request_stays_rangeless() {
        let request = GetAddressDeltasParams::new_address("t1a").into_domain();

        assert!(matches!(
            request,
            domain::rpc::AddressDeltasRequest::Address(_)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_delta_with_block_index(i: u32, bi: Option<u32>) -> AddressDelta {
        AddressDelta {
            satoshis: if i.is_multiple_of(2) { 1_000 } else { -500 },
            txid: format!("deadbeef{:02x}", i),
            index: i,
            height: 123_456 + i,
            address: format!("tmSampleAddress{:02}", i),
            block_index: bi,
        }
    }

    mod serde {
        mod params {
            use serde_json::{json, Value};

            use crate::rpc::jsonrpc::wire::address_deltas::GetAddressDeltasParams;

            #[test]
            fn params_deser_filtered_with_camel_case_and_defaults() {
                let json_value = json!({
                    "addresses": ["tmA", "tmB"],
                    "start": 1000,
                    "end": 0,
                    "chainInfo": true
                });

                let params: GetAddressDeltasParams =
                    serde_json::from_value(json_value).expect("deserialize Filtered");
                match params {
                    GetAddressDeltasParams::Filtered {
                        addresses,
                        start,
                        end,
                        chain_info,
                    } => {
                        assert_eq!(addresses, vec!["tmA".to_string(), "tmB".to_string()]);
                        assert_eq!(start, 1000);
                        assert_eq!(end, 0);
                        assert!(chain_info);
                    }
                    _ => panic!("expected Filtered variant"),
                }
            }

            #[test]
            fn params_deser_filtered_defaults_when_missing() {
                // Only required field is addresses. Others default to 0/false.
                let json_value = json!({ "addresses": ["tmOnly"] });
                let params: GetAddressDeltasParams =
                    serde_json::from_value(json_value).expect("deserialize Filtered minimal");
                match params {
                    GetAddressDeltasParams::Filtered {
                        addresses,
                        start,
                        end,
                        chain_info,
                    } => {
                        assert_eq!(addresses, vec!["tmOnly".to_string()]);
                        assert_eq!(start, 0);
                        assert_eq!(end, 0);
                        assert!(!chain_info);
                    }
                    _ => panic!("expected Filtered variant"),
                }
            }

            #[test]
            fn params_deser_single_address_variant() {
                let json_value = Value::String("tmSingleAddress".into());
                let params: GetAddressDeltasParams =
                    serde_json::from_value(json_value).expect("deserialize Address");
                match params {
                    GetAddressDeltasParams::Address(s) => assert_eq!(s, "tmSingleAddress"),
                    _ => panic!("expected Address variant"),
                }
            }

            #[test]
            fn params_ser_filtered_has_expected_keys_no_block_index() {
                let params =
                    GetAddressDeltasParams::new_filtered(vec!["tmA".into()], 100, 200, true);
                let json_value = serde_json::to_value(&params).expect("serialize");
                let json_object = json_value.as_object().expect("object");
                assert!(json_object.get("addresses").is_some());
                assert_eq!(json_object.get("start").and_then(Value::as_u64), Some(100));
                assert_eq!(json_object.get("end").and_then(Value::as_u64), Some(200));
                assert!(json_object.get("chainInfo").is_some());

                // Critically: no blockindex in params
                assert!(json_object.get("blockindex").is_none());
            }
        }
        mod address_delta {
            use serde_json::Value;

            use crate::rpc::jsonrpc::wire::address_deltas::{
                tests::sample_delta_with_block_index, AddressDelta,
            };

            #[test]
            fn address_delta_ser_deser_roundtrip_with_block_index() {
                let delta_0 = sample_delta_with_block_index(0, Some(7));
                let json_str = serde_json::to_string(&delta_0).expect("serialize delta");
                let delta_1: AddressDelta =
                    serde_json::from_str(&json_str).expect("deserialize delta");
                assert_eq!(delta_0, delta_1);

                // JSON contains the key with the value
                let json_value: Value = serde_json::from_str(&json_str).unwrap();
                assert_eq!(
                    json_value.get("blockindex").and_then(Value::as_u64),
                    Some(7)
                );
            }

            #[test]
            fn address_delta_ser_deser_roundtrip_without_block_index() {
                let delta_0 = sample_delta_with_block_index(1, None);
                let json_str = serde_json::to_string(&delta_0).expect("serialize delta");
                let delta_1: AddressDelta =
                    serde_json::from_str(&json_str).expect("deserialize delta");
                assert_eq!(delta_0, delta_1);

                let json_value: Value = serde_json::from_str(&json_str).unwrap();
                match json_value.get("blockindex") {
                    None => {} // Omitted
                    Some(val) => assert!(val.is_null(), "if present, it should be null when None"),
                }
            }
        }

        mod response {
            use serde_json::{json, Value};

            use crate::rpc::jsonrpc::wire::address_deltas::{
                tests::sample_delta_with_block_index, BlockInfo, GetAddressDeltasResponse,
            };

            #[test]
            fn response_ser_simple_array_shape_includes_delta_block_index() {
                let deltas = vec![
                    sample_delta_with_block_index(0, Some(2)),
                    sample_delta_with_block_index(1, None),
                ];
                let resp = GetAddressDeltasResponse::Simple(deltas.clone());
                let json_value = serde_json::to_value(&resp).expect("serialize response");
                assert!(
                    json_value.is_array(),
                    "Simple response must be a JSON array"
                );
                let json_array = json_value.as_array().unwrap();
                assert_eq!(json_array.len(), deltas.len());

                // First delta has blockindex = 2
                assert_eq!(
                    json_array[0].get("blockindex").and_then(Value::as_u64),
                    Some(2)
                );

                // Second delta may omit or null blockindex
                match json_array[1].get("blockindex") {
                    None => {}
                    Some(val) => assert!(val.is_null()),
                }
            }

            #[test]
            fn response_ser_with_chain_info_shape_deltas_carry_block_index() {
                let source_deltas = vec![
                    sample_delta_with_block_index(2, Some(5)),
                    sample_delta_with_block_index(3, None),
                ];
                let start = BlockInfo {
                    hash: "00..aa".into(),
                    height: 1000,
                };
                let end = BlockInfo {
                    hash: "00..bb".into(),
                    height: 2000,
                };
                let response = GetAddressDeltasResponse::WithChainInfo {
                    deltas: source_deltas,
                    start,
                    end,
                };

                let json_value = serde_json::to_value(&response).expect("serialize response");
                let json_object = json_value.as_object().expect("object");
                assert!(json_object.get("deltas").is_some());
                assert!(json_object.get("start").is_some());
                assert!(json_object.get("end").is_some());

                let deltas = json_object
                    .get("deltas")
                    .unwrap()
                    .as_array()
                    .expect("deltas array");

                // First delta has blockindex=5
                assert_eq!(deltas[0].get("blockindex").and_then(Value::as_u64), Some(5));

                // Second delta may omit or null blockindex
                match deltas[1].get("blockindex") {
                    None => {}
                    Some(val) => assert!(val.is_null()),
                }

                assert!(json_object.get("blockindex").is_none());
                assert!(json_object.get("blockindex").is_none());
            }

            #[test]
            fn response_deser_simple_from_array_with_and_without_block_index() {
                let deltas_source = json!([
                    {
                        "satoshis": 1000,
                        "txid": "deadbeef00",
                        "index": 0,
                        "height": 123456,
                        "address": "tmX",
                        "blockindex": 9
                    },
                    {
                        "satoshis": -500,
                        "txid": "deadbeef01",
                        "index": 1,
                        "height": 123457,
                        "address": "tmY"
                        // blockindex missing
                    }
                ]);
                let response: GetAddressDeltasResponse =
                    serde_json::from_value(deltas_source).expect("deserialize simple");
                match response {
                    GetAddressDeltasResponse::Simple(ds) => {
                        assert_eq!(ds.len(), 2);
                        assert_eq!(ds[0].txid, "deadbeef00");
                        assert_eq!(ds[0].block_index, Some(9));
                        assert_eq!(ds[1].txid, "deadbeef01");
                        assert_eq!(ds[1].block_index, None);
                    }
                    _ => panic!("expected Simple variant"),
                }
            }

            #[test]
            fn response_deser_with_chain_info_from_object_delays_block_index_per_delta() {
                let deltas_source = json!({
                    "deltas": [{
                        "satoshis": -500,
                        "txid": "deadbeef02",
                        "index": 1,
                        "height": 123457,
                        "address": "tmY",
                        "blockindex": 4
                    }, {
                        "satoshis": 2500,
                        "txid": "deadbeef03",
                        "index": 2,
                        "height": 123458,
                        "address": "tmZ"
                        // no blockindex
                    }],
                    "start": { "hash": "aa", "height": 1000 },
                    "end":   { "hash": "bb", "height": 2000 }
                });
                let response: GetAddressDeltasResponse =
                    serde_json::from_value(deltas_source).expect("deserialize with chain info");
                match response {
                    GetAddressDeltasResponse::WithChainInfo { deltas, start, end } => {
                        assert_eq!(deltas.len(), 2);
                        assert_eq!(deltas[0].block_index, Some(4));
                        assert_eq!(deltas[1].block_index, None);
                        assert_eq!(start.height, 1000);
                        assert_eq!(end.height, 2000);
                    }
                    _ => panic!("expected WithChainInfo variant"),
                }
            }
        }
    }
}
