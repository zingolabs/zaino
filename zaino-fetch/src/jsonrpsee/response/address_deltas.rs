//! Types associated with the `getaddressdeltas` RPC request.

use serde::{Deserialize, Serialize};
use zebra_rpc::client::{Input, Output, TransactionObject};

use crate::jsonrpsee::connector::{ResponseToError, RpcError};

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
    /// Processes transaction objects into address deltas for specific addresses.
    /// This is a pure function that can be easily unit tested.
    pub fn process_transactions_to_deltas(
        transactions: &[Box<TransactionObject>],
        target_addresses: &[String],
    ) -> Vec<AddressDelta> {
        let mut deltas: Vec<AddressDelta> = transactions
            .iter()
            .filter(|tx| tx.height().unwrap_or(0) > 0)
            .flat_map(|tx| {
                let txid = tx.txid().to_string();
                let height = tx.height().unwrap(); // height > 0 due to previous filter

                // Inputs (negative deltas)
                let input_deltas = tx.inputs().iter().enumerate().filter_map({
                    let input_txid = txid.clone();
                    move |(input_index, input)| {
                        AddressDelta::from_input(
                            input,
                            input_index as u32,
                            &input_txid,
                            height,
                            target_addresses,
                            None,
                        )
                    }
                });

                // Outputs (positive deltas)
                let output_deltas = tx.outputs().iter().flat_map({
                    let output_txid = txid;
                    move |output| {
                        AddressDelta::from_output(
                            output,
                            &output_txid,
                            height,
                            target_addresses,
                            None,
                        )
                    }
                });

                input_deltas.chain(output_deltas)
            })
            .collect();
        // zcashd-like ordering: (height ASC, blockindex ASC, index ASC)
        deltas.sort_by_key(|d| (d.height, d.block_index.unwrap_or(u32::MAX), d.index));
        deltas
    }
}

/// Error type used for the `getaddressdeltas` RPC request.
#[derive(Debug, thiserror::Error)]
pub enum GetAddressDeltasError {
    /// Invalid encoding
    #[error("Invalid encoding: {0}")]
    InvalidEncoding(String),

    /// Wrong block range
    #[error("Invalid block range. Start = {0}, End = {1}")]
    InvalidBlockRange(u32, u32),
}

impl ResponseToError for GetAddressDeltasResponse {
    type RpcError = GetAddressDeltasError;
}

impl TryFrom<RpcError> for GetAddressDeltasError {
    type Error = RpcError;

    fn try_from(value: RpcError) -> Result<Self, Self::Error> {
        Err(value)
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
    /// Create a delta from a transaction input (spend - negative value)
    pub fn from_input(
        input: &Input,
        input_index: u32,
        txid: &str,
        height: u32,
        target_addresses: &[String],
        block_index: Option<u32>,
    ) -> Option<Self> {
        match input {
            Input::NonCoinbase {
                address: Some(addr),
                value_zat: Some(value),
                ..
            } => {
                // Check if this address is in our target addresses
                if target_addresses.iter().any(|req_addr| req_addr == addr) {
                    Some(AddressDelta {
                        satoshis: -value, // Negative for inputs (spends)
                        txid: txid.to_string(),
                        index: input_index,
                        height,
                        address: addr.clone(),
                        block_index,
                    })
                } else {
                    None
                }
            }
            _ => None, // Skip coinbase inputs or inputs without address/value
        }
    }

    /// Create a delta from a transaction output (receive - positive value)
    pub fn from_output(
        output: &Output,
        txid: &str,
        height: u32,
        target_addresses: &[String],
        block_index: Option<u32>,
    ) -> Vec<Self> {
        if let Some(output_addresses) = &output.script_pub_key().addresses() {
            output_addresses
                .iter()
                .filter(|addr| target_addresses.iter().any(|req_addr| req_addr == *addr))
                .map(|addr| AddressDelta {
                    satoshis: output.value_zat(), // Positive for outputs (receives)
                    txid: txid.to_string(),
                    index: output.n(),
                    height,
                    address: addr.clone(),
                    block_index,
                })
                .collect()
        } else {
            Vec::new()
        }
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

            use crate::jsonrpsee::response::address_deltas::GetAddressDeltasParams;

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

            use crate::jsonrpsee::response::address_deltas::{
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

            use crate::jsonrpsee::response::address_deltas::{
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
