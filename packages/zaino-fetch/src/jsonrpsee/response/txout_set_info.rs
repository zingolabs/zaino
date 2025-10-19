//! Types associated with the `gettxoutsetinfo` RPC request.
//!
//! Although the current threat model assumes that `zaino` connects to a trusted validator,
//! the `gettxoutsetinfo` RPC performs some light validation.

use std::convert::Infallible;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jsonrpsee::{
    connector::ResponseToError,
    response::common::{amount::ZecAmount, BlockHeight},
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
    #[serde(with = "hex", rename = "bestblock")]
    pub best_block: zebra_chain::block::Hash,

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
pub mod utxo_set_hash {
    use std::collections::BTreeMap;

    use zebra_chain::amount::MAX_MONEY;

    pub(crate) const DOMAIN_TAG: &[u8] = b"ZAINO-UTXOSET-V1\0";
    pub(crate) const NETWORK_TAG_LEN: u64 = b"mainnet".len() as u64; // Same as testnet and regtest
    pub(crate) const NETWORK_TAG_NUL: &[u8] = b"\0";

    /// A single UTXO snapshot item.
    #[derive(Debug, Clone)]
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
    pub(crate) fn write_compact_size(size: u64, h: &mut blake3::Hasher) {
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

    /// Return the number of bytes needed to encode a CompactSize varint.
    #[inline]
    pub(crate) fn compact_size_len(n: u64) -> u64 {
        if n < 0xFD {
            1
        } else if n <= 0xFFFF {
            3
        } else if n <= 0xFFFF_FFFF {
            5
        } else {
            9
        }
    }

    /// Error type for UTXO set hash computation.
    #[derive(Debug, thiserror::Error)]
    pub enum UtxoSetError {
        /// Duplicate outpoint in the UTXO set.
        #[error("duplicate outpoint")]
        DuplicateOutpoint,
    }

    /// Compute canonical snapshot hash. See ZAINO-UTXOSET-01 for details.
    ///
    /// - `genesis_block_hash`: raw 32-byte genesis block hash.
    /// - `best_height`: current block height.
    /// - `best_block_hash`: raw 32-byte block hash.
    /// - `items`: anything that can be iterated, we'll sort it into BTreeMap to canonicalize order.
    pub fn utxo_set_hash_v1<I>(
        genesis_block_hash: [u8; 32],
        best_height: u32,
        best_block_hash: [u8; 32],
        items: I,
    ) -> Result<blake3::Hash, UtxoSetError>
    where
        I: IntoIterator<Item = SnapshotItem>,
    {
        // Group by txid, then by index to detect duplicates.
        let mut ordered: BTreeMap<[u8; 32], BTreeMap<u32, SnapshotItem>> = BTreeMap::new();
        for it in items {
            let per_tx = ordered.entry(it.txid_raw).or_default();

            if per_tx.contains_key(&it.index) {
                return Err(UtxoSetError::DuplicateOutpoint);
            }
            per_tx.insert(it.index, it);
        }

        let mut h = blake3::Hasher::new();

        // Header, with domain separation and metadata
        h.update(DOMAIN_TAG);
        h.update(&genesis_block_hash);
        h.update(&best_height.to_le_bytes());
        h.update(&best_block_hash);
        let total_outputs: u64 = ordered.values().map(|v| v.len() as u64).sum();
        h.update(&total_outputs.to_le_bytes());

        // Entries (txid asc, vout asc)
        for (txid, outs) in ordered {
            for (index, o) in outs {
                // Range check value
                assert!(
                    o.value_zat <= MAX_MONEY.try_into().unwrap(),
                    "value_zat out of range"
                );
                h.update(&txid);
                h.update(&index.to_le_bytes());
                h.update(&o.value_zat.to_le_bytes());
                let slen = u64::try_from(o.script.len()).expect("script too long");
                write_compact_size(slen, &mut h);
                h.update(&o.script);
            }
        }

        Ok(h.finalize())
    }

    /// Compute the serialized size (in bytes) of the UTXO set snapshot using the same
    /// deterministic V1 format as `utxo_set_hash_v1`.
    ///
    /// This MUST stay in lockstep with the hashing/serialization spec:
    /// - Header: "ZAINO-UTXOSET-V1\0" || `network` || `NUL` || `height_le` || `best_block` || `total_outputs_le`
    /// - Entries (sorted by `txid` asc, `index` asc): `txid` || `index_le` || `value_le` || `CompactSize(script_len)` || `script`
    pub fn utxo_set_serialized_size_v1<I>(items: I) -> u64
    where
        I: IntoIterator<Item = SnapshotItem>,
    {
        // Header size, independent of contents
        let mut total = 0u64;
        total += DOMAIN_TAG.len() as u64;
        total += NETWORK_TAG_LEN;
        total += NETWORK_TAG_NUL.len() as u64;
        total += std::mem::size_of::<u32>() as u64; // best_height
        total += std::mem::size_of::<[u8; 32]>() as u64; // best_block_hash
        total += std::mem::size_of::<u64>() as u64; // total_outputs

        // Canonicalize order to match hashing (not strictly necessary for size, but keeps invariants).
        use std::collections::BTreeMap;
        let mut ordered: BTreeMap<[u8; 32], Vec<SnapshotItem>> = BTreeMap::new();

        for item in items {
            ordered.entry(item.txid_raw).or_default().push(item);
        }
        for v in ordered.values_mut() {
            v.sort_by_key(|x| x.index);
        }

        for outs in ordered.into_values() {
            for o in outs {
                total += 32; // txid
                total += 4; // index
                total += 8; // value_zat
                let slen = o.script.len() as u64;
                total += compact_size_len(slen); // varint length
                total += slen; // script bytes
            }
        }

        total
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::jsonrpsee::response::txout_set_info::{GetTxOutSetInfo, TxOutSetInfo};

    const GENESIS_BLOCK_HASH: &str =
        "00040fe8ec8471911baa1db1266ea15dd06b4a8a5c453883c000b031973dce08";

    #[test]
    fn txoutsetinfo_parses_known_with_numeric_amount() {
        // `zcashd` payload, with the amount as a number
        let j = json!({
            "height": 123,
            "bestblock": "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
            "transactions": 42,
            "txouts": 77,
            "bytes_serialized": 999,
            "hash_serialized": "c26d00...718f",
            "total_amount": 3.5
        });

        let parsed: GetTxOutSetInfo = serde_json::from_value(j).unwrap();
        match parsed {
            GetTxOutSetInfo::Known(k) => {
                assert_eq!(k.height.0, 123);
                assert_eq!(k.transactions, 42);
                assert_eq!(k.tx_outs, 77);
                assert_eq!(k.bytes_serialized, 999);
                assert_eq!(k.hash_serialized, "c26d00...718f");
                assert_eq!(u64::from(k.total_amount), 350_000_000);

                // BlockHash round-trip formatting is stable
                let hex = k.best_block.to_string();
                assert_eq!(hex.len(), 64);
                let back: zebra_chain::block::Hash = hex.parse().unwrap();
                assert_eq!(k.best_block, back);
            }
            other => panic!("expected Known, got: {:?}", other),
        }
    }

    #[test]
    fn txoutsetinfo_parses_known_with_string_amount() {
        // Amount as a string is accepted
        let known_json = json!({
            "height": 0,
            "bestblock": "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "transactions": 0,
            "txouts": 0,
            "bytes_serialized": 0,
            "hash_serialized": "deadbeef",
            "total_amount": "0.00000001"
        });

        match serde_json::from_value::<TxOutSetInfo>(known_json) {
            Ok(k) => {
                assert_eq!(k.height.0, 0);
                assert_eq!(u64::from(k.total_amount), 1);
            }
            Err(e) => {
                panic!("expected Ok, got: {}", e);
            }
        }
    }

    /// Missing 'bestblock'. Should deserialize to [`GetTxOutSetInfo::Unknown`].
    #[test]
    fn txoutsetinfo_falls_back_to_unknown_when_required_fields_missing() {
        let j = json!({
            "height": 1,
            "transactions": 0,
            "txouts": 0,
            "bytes_serialized": 0,
            "hash_serialized": "x",
            "total_amount": 0
        });

        let parsed: GetTxOutSetInfo = serde_json::from_value(j).unwrap();
        match parsed {
            GetTxOutSetInfo::Unknown(v) => {
                assert!(v.get("bestblock").is_none());
            }
            other => panic!("expected Unknown, got: {:?}", other),
        }
    }

    /// UTXO Set Hash tests
    ///
    /// For more information, see `ZAINO-UTXOSET-01`.
    mod utxoset_hash {
        use crate::jsonrpsee::response::txout_set_info::tests::GENESIS_BLOCK_HASH;

        use super::super::utxo_set_hash::*;
        use blake3;

        fn txid(fill: u8) -> [u8; 32] {
            [fill; 32]
        }

        fn script(len: usize, byte: u8) -> Vec<u8> {
            vec![byte; len]
        }

        fn item(fill: u8, index: u32, value_zat: u64, script_len: usize) -> SnapshotItem {
            SnapshotItem {
                txid_raw: txid(fill),
                index,
                value_zat,
                script: script(script_len, 0xAA),
            }
        }

        #[test]
        fn compact_size_encoding_edges() {
            // Verify that the CompactSize encoder emits the exact byte sequences
            // for boundaries: <0xFD, 0xFD..=0xFFFF, 0x10000..=0xFFFF_FFFF, >= 2^32.
            fn expected_bytes(n: u64) -> Vec<u8> {
                if n < 0xFD {
                    vec![n as u8]
                } else if n <= 0xFFFF {
                    vec![0xFD, (n & 0xFF) as u8, ((n >> 8) & 0xFF) as u8]
                } else if n <= 0xFFFF_FFFF {
                    vec![
                        0xFE,
                        (n & 0xFF) as u8,
                        ((n >> 8) & 0xFF) as u8,
                        ((n >> 16) & 0xFF) as u8,
                        ((n >> 24) & 0xFF) as u8,
                    ]
                } else {
                    vec![
                        0xFF,
                        (n & 0xFF) as u8,
                        ((n >> 8) & 0xFF) as u8,
                        ((n >> 16) & 0xFF) as u8,
                        ((n >> 24) & 0xFF) as u8,
                        ((n >> 32) & 0xFF) as u8,
                        ((n >> 40) & 0xFF) as u8,
                        ((n >> 48) & 0xFF) as u8,
                        ((n >> 56) & 0xFF) as u8,
                    ]
                }
            }

            for &n in &[
                0u64,
                1,
                252,
                253,
                65535,
                65536,
                4_294_967_295,
                4_294_967_296,
            ] {
                let mut h1 = blake3::Hasher::new();
                write_compact_size(n, &mut h1);
                let got = h1.finalize();

                let mut h2 = blake3::Hasher::new();
                h2.update(&expected_bytes(n));
                let exp = h2.finalize();

                assert_eq!(got, exp, "mismatch for n={n}");
            }
        }

        /// Same logical set, different insertion orders. Should get the same digest.
        #[test]
        fn utxo_set_hash_is_deterministic_and_order_canonicalized() {
            let best_block_hash = [0x11; 32];

            let item_a = vec![
                item(0xAA, 0, 50, 10),
                item(0xAA, 1, 60, 0),
                item(0xBB, 0, 70, 3),
            ];

            // Shuffled order
            let item_b = vec![
                item(0xBB, 0, 70, 3),
                item(0xAA, 1, 60, 0),
                item(0xAA, 0, 50, 10),
            ];

            let hash_1 = utxo_set_hash_v1(
                hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                100,
                best_block_hash,
                item_a,
            )
            .unwrap();
            let hash_2 = utxo_set_hash_v1(
                hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                100,
                best_block_hash,
                item_b,
            )
            .unwrap();

            assert_eq!(hash_1, hash_2);
        }

        #[test]
        fn utxo_set_hash_changes_with_header_fields() {
            let items = [item(0x01, 0, 1, 0)];
            let best_block_hash_1 = [0x22; 32];
            let best_block_hash_2 = [0x23; 32];

            let different_genesis_hash = [0x00; 32];

            let base = utxo_set_hash_v1(
                hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                1,
                best_block_hash_1,
                items.clone(),
            )
            .unwrap();
            assert_ne!(
                base,
                utxo_set_hash_v1(different_genesis_hash, 1, best_block_hash_1, items.clone())
                    .unwrap(),
                "network must affect hash"
            );
            assert_ne!(
                base,
                utxo_set_hash_v1(
                    hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                    2,
                    best_block_hash_1,
                    items.clone()
                )
                .unwrap(),
                "height must affect hash"
            );
            assert_ne!(
                base,
                utxo_set_hash_v1(
                    hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                    1,
                    best_block_hash_2,
                    items.clone()
                )
                .unwrap(),
                "best_block must affect hash"
            );
        }

        #[test]
        fn utxo_set_hash_changes_when_entry_changes() {
            let best_block_hash = [0x99; 32];
            let custom_genesis_hash = [0x00; 32];

            let base = utxo_set_hash_v1(
                custom_genesis_hash,
                123,
                best_block_hash,
                [item(0x10, 0, 1_000, 5)],
            )
            .unwrap();

            // Change value
            let h_val = utxo_set_hash_v1(
                custom_genesis_hash,
                123,
                best_block_hash,
                [item(0x10, 0, 2_000, 5)],
            )
            .unwrap();
            assert_ne!(base, h_val);

            // Change index
            let h_idx = utxo_set_hash_v1(
                custom_genesis_hash,
                123,
                best_block_hash,
                [item(0x10, 1, 1_000, 5)],
            )
            .unwrap();
            assert_ne!(base, h_idx);

            // Change script content/length
            let h_scr = utxo_set_hash_v1(
                custom_genesis_hash,
                123,
                best_block_hash,
                [item(0x10, 0, 1_000, 6)],
            )
            .unwrap();
            assert_ne!(base, h_scr);
        }

        #[test]
        fn utxo_set_compactsize_boundary_lengths_affect_hash() {
            // Check that going from 252 to 253 (boundary into 0xFD form) changes the hash.
            let best_block_hash = [0x55; 32];

            let h_252 = utxo_set_hash_v1(
                hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                7,
                best_block_hash,
                [SnapshotItem {
                    txid_raw: [1; 32],
                    index: 0,
                    value_zat: 42,
                    script: vec![0xAA; 252],
                }],
            )
            .unwrap();
            let h_253 = utxo_set_hash_v1(
                hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                7,
                best_block_hash,
                [SnapshotItem {
                    txid_raw: [1; 32],
                    index: 0,
                    value_zat: 42,
                    script: vec![0xAA; 253],
                }],
            )
            .unwrap();

            assert_ne!(h_252, h_253, "length prefix must change at boundary");
        }

        #[test]
        fn duplicate_outpoint_returns_error() {
            let mut txid = [0u8; 32];
            txid[0] = 1;

            let a = SnapshotItem {
                txid_raw: txid,
                index: 0,
                value_zat: 1,
                script: vec![],
            };
            let b = SnapshotItem {
                txid_raw: txid,
                index: 0,
                value_zat: 2,
                script: vec![0x51],
            }; // same (txid,vout)

            let genesis = [9u8; 32];
            let best = [8u8; 32];

            let res = utxo_set_hash_v1(genesis, 123, best, vec![a, b]);

            assert!(matches!(res, Err(UtxoSetError::DuplicateOutpoint)));
        }
    }

    mod byte_order_tests {

        use crate::jsonrpsee::response::{
            common::{amount::ZecAmount, BlockHeight},
            txout_set_info::{
                tests::GENESIS_BLOCK_HASH,
                utxo_set_hash::{utxo_set_hash_v1, SnapshotItem, DOMAIN_TAG},
                TxOutSetInfo,
            },
        };

        /// Return a sequence of bytes with a known display order.
        fn seq_bytes() -> [u8; 32] {
            let mut bytes = [0u8; 32];
            for (i, x) in bytes.iter_mut().enumerate() {
                *x = i as u8;
            }
            bytes
        }

        #[test]
        fn utxo_set_header_uses_display_order_bytes() {
            let height = 123u32;
            let display_bytes = seq_bytes();

            let genesis_block_hash_bytes: [u8; 32] =
                hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap();

            let h_func = utxo_set_hash_v1(
                genesis_block_hash_bytes,
                height,
                display_bytes,
                std::iter::empty::<SnapshotItem>(),
            )
            .unwrap();

            let mut hasher = blake3::Hasher::new();
            hasher.update(DOMAIN_TAG);
            hasher.update(&genesis_block_hash_bytes);
            hasher.update(&height.to_le_bytes());
            hasher.update(&display_bytes);
            hasher.update(&0u64.to_le_bytes());
            let h_manual = hasher.finalize();

            assert_eq!(
                h_func, h_manual,
                "header must be fed with display-order bytes"
            );
        }

        #[test]
        fn wrong_endianness_changes_digest() {
            let height = 7u32;
            let display_bytes = seq_bytes();

            let h_ok = utxo_set_hash_v1(
                hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                height,
                display_bytes,
                std::iter::empty::<SnapshotItem>(),
            )
            .unwrap();

            let mut flipped = display_bytes;
            flipped.reverse();
            let h_bad = utxo_set_hash_v1(
                hex::decode(GENESIS_BLOCK_HASH).unwrap().try_into().unwrap(),
                height,
                flipped,
                std::iter::empty::<SnapshotItem>(),
            )
            .unwrap();

            assert_ne!(
                h_ok, h_bad,
                "feeding non-display-order bytes must change the digest"
            );
        }

        #[test]
        fn bestblock_string_matches_bytes_used_in_hash() {
            let height = 0u32;
            let best_block = seq_bytes();
            let best_block_hex = hex::encode(best_block);

            // Compute digest using display-order bytes
            let digest = utxo_set_hash_v1(
                [0x00; 32],
                height,
                best_block,
                std::iter::empty::<SnapshotItem>(),
            )
            .unwrap();

            // SAFEST construction: go through the hex string, so display is as stored
            let best_block: zebra_chain::block::Hash =
                best_block_hex.parse().expect("valid 32-byte hex");

            let info = TxOutSetInfo {
                height: BlockHeight(height),
                best_block,
                transactions: 0,
                tx_outs: 0,
                bytes_serialized: 0,
                hash_serialized: digest.to_string(),
                total_amount: ZecAmount::from_zats(0),
            };
            let v = serde_json::to_value(&info).unwrap();

            // The JSON should carry the same hex we hashed in the header.
            assert_eq!(v["bestblock"].as_str().unwrap(), best_block_hex);
        }
    }

    mod size_tests {
        use crate::jsonrpsee::response::txout_set_info::utxo_set_hash::{
            utxo_set_serialized_size_v1, SnapshotItem, DOMAIN_TAG, NETWORK_TAG_LEN, NETWORK_TAG_NUL,
        };

        #[test]
        fn header_only_size_is_constant_plus_network() {
            let size = utxo_set_serialized_size_v1(std::iter::empty::<SnapshotItem>());
            let expected = DOMAIN_TAG.len() as u64
            + NETWORK_TAG_LEN
            + NETWORK_TAG_NUL.len() as u64
            + 4 // u32 height
            + 32
            + 8; // total_outputs u64
            assert_eq!(size, expected);
        }

        #[test]
        fn single_entry_zero_script_size_matches_manual() {
            let net = "regtest";
            let best_block = [0xAA; 32];
            let item = SnapshotItem {
                txid_raw: [0x11; 32],
                index: 2,
                value_zat: 42,
                script: vec![],
            };

            let counted = utxo_set_serialized_size_v1([item]);

            let mut bytes = Vec::new();
            bytes.extend_from_slice(DOMAIN_TAG);
            bytes.extend_from_slice(net.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&7u32.to_le_bytes());
            bytes.extend_from_slice(&best_block);
            bytes.extend_from_slice(&1u64.to_le_bytes()); // total_outputs = 1

            // entry
            bytes.extend_from_slice(&[0x11; 32]);
            bytes.extend_from_slice(&2u32.to_le_bytes());
            bytes.extend_from_slice(&42u64.to_le_bytes());
            bytes.push(0); // No script bytes

            assert_eq!(counted as usize, bytes.len());
        }

        #[test]
        fn compactsize_thresholds_change_size() {
            let mk_item = |len: usize| SnapshotItem {
                txid_raw: [0u8; 32],
                index: 0,
                value_zat: 0,
                script: vec![0xAA; len],
            };

            let with_len = |len| utxo_set_serialized_size_v1([mk_item(len)]);

            // 1 byte varint
            let varint_a = with_len(252);
            // 3 bytes varint
            let varint_b = with_len(253);
            assert_eq!(varint_b - varint_a, 3);

            // 3 bytes varint
            let varint_c = with_len(65535);
            // 5 bytes varint
            let varint_d = with_len(65536);
            assert_eq!(varint_d - varint_c, 3);
        }

        #[test]
        fn multiple_entries_add_linearly() {
            let base = SnapshotItem {
                txid_raw: [0x77; 32],
                index: 1,
                value_zat: 1_000,
                script: vec![1, 2, 3, 4], // CompactSize = 1
            };

            let size_1 = utxo_set_serialized_size_v1([base.clone()]);
            let size_2 = utxo_set_serialized_size_v1([base.clone(), base.clone()]);
            assert_eq!(
                size_2 - size_1,
                size_1
                    - (DOMAIN_TAG.len() as u64
                        + NETWORK_TAG_LEN
                        + NETWORK_TAG_NUL.len() as u64
                        + 4
                        + 32
                        + 8)
            );
        }
    }
}
