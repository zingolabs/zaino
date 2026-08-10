//! Wire shapes for the zcashd-only methods too small to warrant their own file.
//!
//! `getmempoolinfo`, `getnetworksolps`, `getspentinfo`, `gettxout` and
//! `gettxoutsetinfo`.

use serde::{Deserialize, Serialize};

use zaino_primitives::types::{
    rpc::{SpentInfo, SpentOutpoint, TxOut},
    TxOutSetInfo,
};

use super::{display_hex, zats_to_zec};

/// Response to `getmempoolinfo`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MempoolInfoWire {
    /// Number of transactions currently in the mempool.
    pub size: u64,
    /// Sum of the serialised sizes of those transactions.
    pub bytes: u64,
    /// Total memory the mempool occupies.
    pub usage: u64,
}

impl MempoolInfoWire {
    /// Renders Zaino's mempool statistics.
    ///
    /// The domain type is `chain_index::types::db::metadata::MempoolInfo`, which
    /// is an on-disk shape rather than one of `zaino-primitives`' types. That is
    /// deliberate for now: it carries a `ZainoVersionedSerde` impl, so moving it
    /// belongs with the persistence rework rather than here.
    pub fn from_domain(info: zaino_state::MempoolInfo) -> Self {
        Self {
            size: info.size,
            bytes: info.bytes,
            usage: info.usage,
        }
    }
}

/// Response to `getnetworksolps`: the estimated network hash rate.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct NetworkSolPsWire(pub u64);

/// Request parameters for `getspentinfo`.
///
/// Arrives from a client, so [`Self::into_domain`] is where the txid is
/// validated.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpentInfoRequestWire {
    /// Hex transaction id, in RPC display order, of the transaction containing
    /// the output.
    pub txid: String,
    /// Index of the output within that transaction's `vout` array.
    pub index: u32,
}

/// Why a [`SpentInfoRequestWire`] could not be understood.
#[derive(Debug, thiserror::Error)]
pub enum SpentInfoRequestError {
    /// The `txid` was not 32 bytes of hex.
    #[error("txid is not a 32-byte hex string: {0}")]
    Txid(String),
}

impl SpentInfoRequestWire {
    /// Validates the request into the outpoint it names.
    pub fn into_domain(self) -> Result<SpentOutpoint, SpentInfoRequestError> {
        let mut bytes = <[u8; 32]>::try_from(
            hex::decode(&self.txid)
                .map_err(|e| SpentInfoRequestError::Txid(e.to_string()))?
                .as_slice(),
        )
        .map_err(|_| SpentInfoRequestError::Txid("expected 32 bytes".to_string()))?;

        // Txids arrive in display order, which is byte-reversed from internal
        // order.
        bytes.reverse();

        Ok(SpentOutpoint {
            txid: bytes.into(),
            index: self.index,
        })
    }
}

/// Response to `getspentinfo`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpentInfoWire {
    /// Transaction that spent the requested output.
    pub txid: String,
    /// Index of the spending input within that transaction's `vin` array.
    pub index: u32,
    /// Height of the block containing the spending transaction.
    ///
    /// Returned by zcashd 6.12.2, though absent from the published 6.2.0 RPC
    /// page.
    pub height: u32,
}

impl SpentInfoWire {
    /// Renders where an output was spent.
    pub fn from_domain(spent: SpentInfo) -> Self {
        Self {
            txid: display_hex(<[u8; 32]>::from(spent.txid)),
            index: spent.index,
            height: u32::from(spent.height),
        }
    }
}

/// Response to `gettxout`.
///
/// Untyped on the wire — the interface specifies an object or JSON `null`, and
/// the object's fields vary by validator. The JSON is therefore built here from
/// the modelled value rather than forwarded, which is what lets the port carry
/// a typed [`TxOut`] instead of opaque JSON.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct TxOutWire(pub Option<serde_json::Value>);

impl TxOutWire {
    /// Renders an unspent output, or JSON `null` if there was none.
    ///
    /// `null` is the interface's real answer to "is this outpoint unspent?", not
    /// an error condition.
    pub fn from_domain(out: Option<TxOut>) -> Self {
        let Some(out) = out else {
            return Self(None);
        };

        let mut script = serde_json::Map::new();
        script.insert(
            "hex".to_string(),
            serde_json::Value::String(hex::encode(Vec::<u8>::from(out.script_pub_key.script))),
        );
        if let Some(asm) = out.script_pub_key.asm {
            script.insert("asm".to_string(), serde_json::Value::String(asm));
        }
        if let Some(kind) = out.script_pub_key.script_type {
            script.insert("type".to_string(), serde_json::Value::String(kind));
        }
        if let Some(required) = out.script_pub_key.required_signatures {
            script.insert("reqSigs".to_string(), serde_json::Value::from(required));
        }
        // Omitted rather than emitted empty: the validator attributing no
        // address is normal for scripts that do not correspond to one, and an
        // empty array would suggest it tried and found none.
        if !out.script_pub_key.addresses.is_empty() {
            script.insert(
                "addresses".to_string(),
                serde_json::Value::Array(
                    out.script_pub_key
                        .addresses
                        .into_iter()
                        .map(|address| serde_json::Value::String(String::from(address)))
                        .collect(),
                ),
            );
        }

        let mut object = serde_json::Map::new();
        object.insert(
            "bestblock".to_string(),
            serde_json::Value::String(display_hex(<[u8; 32]>::from(out.best_block))),
        );
        object.insert(
            "confirmations".to_string(),
            serde_json::Value::from(out.confirmations),
        );
        object.insert(
            "value".to_string(),
            serde_json::Value::from(zats_to_zec(out.value)),
        );
        object.insert(
            "scriptPubKey".to_string(),
            serde_json::Value::Object(script),
        );
        object.insert(
            "coinbase".to_string(),
            serde_json::Value::Bool(out.coinbase),
        );

        Self(Some(serde_json::Value::Object(object)))
    }
}

/// Response to `gettxoutsetinfo`.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TxOutSetInfoWire {
    /// UTXO set statistics.
    Info(TxOutSetInfoStats),
    /// An empty object. zcashd answers this way when it cannot collect the
    /// statistics; Zaino does the same while its accumulator is still syncing.
    Empty(EmptyTxOutSetInfo),
}

impl TxOutSetInfoWire {
    /// Renders UTXO set statistics, or the empty object if there are none.
    pub fn from_domain(info: Option<TxOutSetInfo>) -> Self {
        match info {
            None => Self::Empty(EmptyTxOutSetInfo {}),
            Some(info) => Self::Info(TxOutSetInfoStats {
                height: u64::from(u32::from(info.height)),
                best_block: display_hex(<[u8; 32]>::from(info.best_block)),
                transactions: info.transactions,
                txouts: info.tx_outs,
                bytes_serialized: info.bytes_serialized,
                hash_serialized: info.hash_serialized,
                total_amount: zats_to_zec(info.total_amount),
            }),
        }
    }
}

/// UTXO set statistics from a successful `gettxoutsetinfo`.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct TxOutSetInfoStats {
    /// Height the statistics were computed at.
    pub height: u64,
    /// Best-chain block they were computed against.
    #[serde(rename = "bestblock")]
    pub best_block: String,
    /// Transactions holding at least one unspent transparent output.
    pub transactions: u64,
    /// Unspent transparent outputs.
    pub txouts: u64,
    /// Serialised size of the UTXO set, in bytes.
    pub bytes_serialized: u64,
    /// Hash over the serialised UTXO set.
    pub hash_serialized: String,
    /// Total value held, in ZEC.
    pub total_amount: f64,
}

/// The empty `gettxoutsetinfo` answer.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct EmptyTxOutSetInfo {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zaino_primitives::types::{
        rpc::ScriptPubKey, Height, Script, TransparentAddress, Zatoshis,
    };

    fn zats(value: u64) -> Zatoshis {
        Zatoshis::new(value).unwrap()
    }

    /// Round-tripping a txid through display order must land on the same bytes:
    /// a missing or doubled reversal names a different transaction.
    #[test]
    fn spent_info_request_reverses_display_order_once() {
        let internal: [u8; 32] = core::array::from_fn(|i| i as u8);
        let display = display_hex(internal);

        let outpoint = SpentInfoRequestWire {
            txid: display,
            index: 3,
        }
        .into_domain()
        .expect("valid txid");

        assert_eq!(<[u8; 32]>::from(outpoint.txid), internal);
        assert_eq!(outpoint.index, 3);
    }

    #[test]
    fn spent_info_request_rejects_malformed_txids() {
        for bad in ["", "not hex", "abcd"] {
            assert!(SpentInfoRequestWire {
                txid: bad.to_string(),
                index: 0,
            }
            .into_domain()
            .is_err());
        }
    }

    /// A spent or unknown outpoint is JSON `null`, not an error and not an
    /// object with empty fields.
    #[test]
    fn tx_out_absent_is_json_null() {
        let wire = TxOutWire::from_domain(None);
        assert_eq!(serde_json::to_value(&wire).unwrap(), json!(null));
    }

    #[test]
    fn tx_out_shape() {
        let wire = TxOutWire::from_domain(Some(TxOut {
            best_block: [0xaa; 32].into(),
            confirmations: 7,
            value: zats(150_000_000),
            script_pub_key: ScriptPubKey {
                script: Script::new(vec![0x76, 0xa9]),
                asm: Some("OP_DUP OP_HASH160".to_string()),
                script_type: Some("pubkeyhash".to_string()),
                required_signatures: Some(1),
                addresses: vec![TransparentAddress::new("t1abc".to_string())],
            },
            coinbase: true,
        }));

        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({
                "bestblock": display_hex([0xaa; 32]),
                "confirmations": 7,
                "value": 1.5,
                "scriptPubKey": {
                    "hex": "76a9",
                    "asm": "OP_DUP OP_HASH160",
                    "type": "pubkeyhash",
                    "reqSigs": 1,
                    "addresses": ["t1abc"],
                },
                "coinbase": true,
            })
        );
    }

    /// A validator that attributes no address must not produce an empty
    /// `addresses` array — the key is absent instead.
    #[test]
    fn tx_out_omits_absent_script_details() {
        let wire = TxOutWire::from_domain(Some(TxOut {
            best_block: [0; 32].into(),
            confirmations: 0,
            value: Zatoshis::ZERO,
            script_pub_key: ScriptPubKey {
                script: Script::new(vec![0x6a]),
                asm: None,
                script_type: None,
                required_signatures: None,
                addresses: Vec::new(),
            },
            coinbase: false,
        }));

        let script = &wire.0.as_ref().unwrap()["scriptPubKey"];
        assert_eq!(script["hex"], "6a");
        for absent in ["asm", "type", "reqSigs", "addresses"] {
            assert_eq!(script.get(absent), None, "{absent} must be omitted");
        }
    }

    /// zcashd's empty answer is `{}`, not `null` and not zeroed statistics.
    #[test]
    fn tx_out_set_info_empty_is_an_empty_object() {
        let wire = TxOutSetInfoWire::from_domain(None);
        assert_eq!(serde_json::to_value(&wire).unwrap(), json!({}));
    }

    #[test]
    fn tx_out_set_info_shape() {
        let wire = TxOutSetInfoWire::from_domain(Some(TxOutSetInfo {
            height: Height::try_from(42).unwrap(),
            best_block: [0xbb; 32].into(),
            transactions: 5,
            tx_outs: 9,
            bytes_serialized: 585,
            hash_serialized: "cc".repeat(32),
            total_amount: zats(250_000_000),
        }));

        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({
                "height": 42,
                "bestblock": display_hex([0xbb; 32]),
                "transactions": 5,
                "txouts": 9,
                "bytes_serialized": 585,
                "hash_serialized": "cc".repeat(32),
                "total_amount": 2.5,
            })
        );
    }

    #[test]
    fn network_sol_ps_is_a_bare_number() {
        assert_eq!(
            serde_json::to_value(NetworkSolPsWire(1_234)).unwrap(),
            json!(1_234)
        );
    }
}
