//! Types associated with the `gettxoutsetinfo` RPC request.
//!
//! Although the current threat model assumes that `zaino` connects to a trusted validator,
//! the `gettxoutsetinfo` RPC performs some light validation.

use std::convert::Infallible;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jsonrpsee::{
    connector::ResponseToError,
    response::common::{amount::ZecAmount, block::BlockHash, BlockHeight},
};

/// Response to a `gettxoutsetinfo` RPC request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum GetTxOutSetInfo {
    /// Validated payload.
    Known(TxOutSetInfo),

    /// Unrecognized shape.
    Unknown(Value),
}

/// Response to a `gettxoutsetinfo` RPC request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxOutSetInfo {
    /// The current block height.
    pub height: BlockHeight,

    /// The best block hash hex.
    #[serde(rename = "bestblock")]
    pub best_block: BlockHash,

    /// The number of transactions.
    pub transactions: u64,

    /// The number of output transactions.
    #[serde(rename = "txouts")]
    pub tx_outs: u64,

    /// The serialized size.
    pub bytes_serialized: u64,

    /// The serialized hash.
    pub hash_serialized: String,

    /// The total amount, in ZEC.
    pub total_amount: ZecAmount,
}

impl ResponseToError for GetTxOutSetInfo {
    type RpcError = Infallible;
}

/// This module provides helper functions and types for computing the canonical UTXO set hash.
pub mod helpers {
    use std::collections::BTreeMap;

    /// A single UTXO snapshot item.
    pub struct SnapshotItem {
        /// Raw txid bytes. Same order everywhere.
        pub txid_raw: [u8; 32],

        /// vout.
        pub index: u32,

        /// Zatoshis.
        pub value_zat: u64,

        /// scriptPubKey.
        pub script: Vec<u8>,
    }

    /// Encode Zcash CompactSize varint.
    fn write_compact_size(size: u64, h: &mut blake3::Hasher) {
        if size < 0xFD {
            h.update(&[size as u8]);
        } else if size <= 0xFFFF {
            h.update(&[0xFD, (size & 0xFF) as u8, ((size >> 8) & 0xFF) as u8]);
        } else if size <= 0xFFFF_FFFF {
            h.update(&[
                0xFE,
                (size & 0xFF) as u8,
                ((size >> 8) & 0xFF) as u8,
                ((size >> 16) & 0xFF) as u8,
                ((size >> 24) & 0xFF) as u8,
            ]);
        } else {
            h.update(&[
                0xFF,
                (size & 0xFF) as u8,
                ((size >> 8) & 0xFF) as u8,
                ((size >> 16) & 0xFF) as u8,
                ((size >> 24) & 0xFF) as u8,
                ((size >> 32) & 0xFF) as u8,
                ((size >> 40) & 0xFF) as u8,
                ((size >> 48) & 0xFF) as u8,
                ((size >> 56) & 0xFF) as u8,
            ]);
        }
    }

    /// Compute canonical snapshot hash. See ZAINO-UHS-01 for details.
    ///
    /// - `network`: "mainnet", "testnet" or "regtest".
    /// - `best_height`: current block height.
    /// - `best_block_hash`: raw 32-byte block hash.
    /// - `items`: anything that can be iterated, we'll sort it into BTreeMap to canonicalize order.
    pub fn utxoset_hash_v1<I>(
        network: &str, // TODO: Use typed enum
        best_height: u32,
        best_block_hash: [u8; 32],
        items: I,
    ) -> blake3::Hash
    where
        I: IntoIterator<Item = SnapshotItem>,
    {
        // Canonical order: txid asc, index asc
        let mut ordered: BTreeMap<[u8; 32], Vec<SnapshotItem>> = BTreeMap::new();
        for it in items {
            ordered.entry(it.txid_raw).or_default().push(it);
        }
        for v in ordered.values_mut() {
            v.sort_by_key(|x| x.index);
        }

        let mut h = blake3::Hasher::new();

        // Header, with domain separation and metadata
        h.update(b"ZAINO-UTXOSET-V1\0");
        h.update(network.as_bytes());
        h.update(&[0]); // NUL
        h.update(&best_height.to_le_bytes());
        h.update(&best_block_hash);
        let total_outputs: u64 = ordered.values().map(|v| v.len() as u64).sum();
        h.update(&total_outputs.to_le_bytes());

        // Entries
        for (txid, outs) in ordered {
            for o in outs {
                h.update(&txid);
                h.update(&o.index.to_le_bytes());
                h.update(&o.value_zat.to_le_bytes());
                write_compact_size(o.script.len() as u64, &mut h);
                h.update(&o.script);
            }
        }

        h.finalize()
    }
}
