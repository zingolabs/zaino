//! AddressHistoryIndex (BlockLocal × Append): transparent address → receives.
//!
//! For each transparent output, records that an address *received* an amount at
//! a location. The **clever key** is address-prefixed —
//! `addr_id ++ height ++ txid ++ output_index` (65 bytes) — so a reader answers a
//! per-address query with one range scan over the `addr_id` prefix (utxos before
//! spend-filtering, the txids that paid an address, gross received). Netting
//! spends (for balance / deltas) is a read-side composition with
//! `TransparentSpendsIndex`; this index is the receive side.
//!
//! `addr_id` is a fixed 21 bytes — `ScriptType` (1) + `hash160` (20) — derived
//! from the output script by `classify_script`. A reader keys by the same id by
//! decoding a queried t-address into `(type, hash160)`, so write and read agree
//! without this index depending on the address-string parser.

use zaino_persistence_codec::{
    decode_key, decode_value, DecodeError, EntryCodec, PersistentRecord,
};
use zaino_primitives::types::{
    classify_script, OutputIndex, Script, ScriptType, TransactionId, Zatoshis,
};
use zaino_sync::backend::{BackendReader, ReadError};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractLocal, IndexDef, MergeAppend, Schema};

/// A transparent output with its index already typed: `(index, value, script)`.
pub type OutputEntry = (OutputIndex, Zatoshis, Script);

/// A transaction's id and its transparent outputs.
pub type TxOutputs = (TransactionId, Vec<OutputEntry>);

/// Per-index context: the block's height and, per transaction, its id and
/// transparent outputs.
///
/// Each output arrives with its `OutputIndex` already typed — a domain fact of
/// the parsed transaction, not something this index re-derives. The
/// `usize → u32` narrowing lives once at the parse boundary (validating the
/// output-count CompactSize from untrusted bytes); downstream it flows typed, so
/// extraction is total.
pub struct AddressCtx {
    /// Block height.
    pub height: BlockHeight,
    /// Per transaction: its id and outputs.
    pub txs: Vec<TxOutputs>,
}

/// A fixed-length transparent address identifier: script type + hash160.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddrId {
    /// Which standard script shape (or non-standard) produced this id.
    pub script_type: ScriptType,
    /// The 20-byte hash the script locks to.
    pub hash: [u8; 20],
}

/// One receive: an address got `value` at this location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressReceive {
    /// Recipient address id.
    pub addr: AddrId,
    /// Block height.
    pub height: BlockHeight,
    /// Transaction that paid the address.
    pub txid: TransactionId,
    /// Output index within that transaction.
    pub output_index: OutputIndex,
    /// Amount received.
    pub value: Zatoshis,
}

/// Persisted key: `addr_id(21) ++ height(8) ++ txid(32) ++ output_index(4)` = 65
/// bytes. Address-prefixed, so a per-address range scan is contiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddrKey {
    /// Recipient address id.
    pub addr: AddrId,
    /// Block height.
    pub height: BlockHeight,
    /// Transaction id.
    pub txid: TransactionId,
    /// Output index.
    pub output_index: OutputIndex,
}

/// Index definition.
pub struct AddressHistoryIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("address_history");

fn script_type_byte(t: ScriptType) -> u8 {
    match t {
        ScriptType::P2PKH => 0,
        ScriptType::P2SH => 1,
        ScriptType::NonStandard => 2,
    }
}

fn script_type_from_byte(b: u8) -> Result<ScriptType, DecodeError> {
    match b {
        0 => Ok(ScriptType::P2PKH),
        1 => Ok(ScriptType::P2SH),
        2 => Ok(ScriptType::NonStandard),
        other => Err(DecodeError::Invalid(format!(
            "invalid script-type byte {other}"
        ))),
    }
}

impl IndexDef for AddressHistoryIndex {
    type Scope = BlockLocal;
    type Composition = Append;
    type Delta = Vec<AddressReceive>;
    type BlockContext = AddressCtx;

    const NAME: IndexId = ID;
}

impl ExtractLocal for AddressHistoryIndex {
    type Error = std::convert::Infallible;

    fn extract(ctx: &AddressCtx) -> Result<Self::Delta, Self::Error> {
        let mut receives = Vec::new();
        for (txid, outputs) in &ctx.txs {
            for (output_index, value, script) in outputs {
                let (hash, script_type) = classify_script(&Vec::<u8>::from(script.clone()));
                receives.push(AddressReceive {
                    addr: AddrId { script_type, hash },
                    height: ctx.height,
                    txid: *txid,
                    output_index: *output_index,
                    value: *value,
                });
            }
        }
        Ok(receives)
    }
}

impl MergeAppend for AddressHistoryIndex {}

impl Schema<Vec<Vec<AddressReceive>>> for AddressHistoryIndex {
    fn into_entries(batches: Vec<Vec<AddressReceive>>) -> Vec<(Self::Key, Self::Value)> {
        batches
            .into_iter()
            .flatten()
            .map(|r| {
                (
                    AddrKey {
                        addr: r.addr,
                        height: r.height,
                        txid: r.txid,
                        output_index: r.output_index,
                    },
                    r.value,
                )
            })
            .collect()
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> Vec<Vec<AddressReceive>> {
        vec![entries
            .into_iter()
            .map(|(key, value)| AddressReceive {
                addr: key.addr,
                height: key.height,
                txid: key.txid,
                output_index: key.output_index,
                value,
            })
            .collect()]
    }
}

impl EntryCodec for AddressHistoryIndex {
    type Key = AddrKey;
    type Value = Zatoshis;
    type PersistentKey = PersistentAddrKey;
    type PersistentValue = PersistentReceiveValue;

    fn fingerprint_samples() -> Vec<(AddrKey, Zatoshis)> {
        // Cover every ScriptType variant, since the type byte is part of the key.
        let sample = |script_type, seed: u8| {
            (
                AddrKey {
                    addr: AddrId {
                        script_type,
                        hash: [seed; 20],
                    },
                    height: BlockHeight::new(u64::from(seed)),
                    txid: TransactionId::from([seed; 32]),
                    output_index: u32::from(seed),
                },
                Zatoshis::new(u64::from(seed) + 1).expect("valid"),
            )
        };
        vec![
            sample(ScriptType::P2PKH, 1),
            sample(ScriptType::P2SH, 2),
            sample(ScriptType::NonStandard, 3),
        ]
    }
}

/// On-disk address-history key:
/// `script_type(1) ++ hash(20) ++ height(8 BE) ++ txid(32) ++ output_index(4 BE)`
/// = 65 bytes.
///
/// Height and output index are **big-endian** on purpose: the record is
/// address-prefixed and byte-lexicographic order must match height order for a
/// per-address range scan, so these two fields carry the `#[persistent(be)]`
/// attribute rather than the little-endian default.
#[derive(PersistentRecord)]
pub struct PersistentAddrKey {
    script_type: u8,
    hash: [u8; 20],
    #[persistent(be)]
    height: u64,
    txid: [u8; 32],
    #[persistent(be)]
    output_index: u32,
}

impl PersistentRecord for PersistentAddrKey {
    type Domain = AddrKey;

    fn from_domain(domain: &AddrKey) -> Self {
        Self {
            script_type: script_type_byte(domain.addr.script_type),
            hash: domain.addr.hash,
            height: domain.height.value(),
            txid: <[u8; 32]>::from(domain.txid),
            output_index: domain.output_index,
        }
    }

    fn into_domain(self) -> Result<AddrKey, DecodeError> {
        let script_type = script_type_from_byte(self.script_type)?;
        Ok(AddrKey {
            addr: AddrId {
                script_type,
                hash: self.hash,
            },
            height: BlockHeight::new(self.height),
            txid: TransactionId::from(self.txid),
            output_index: self.output_index,
        })
    }
}

/// On-disk received-amount record: a single `u64` little-endian zatoshi count.
#[derive(PersistentRecord)]
pub struct PersistentReceiveValue(u64);

impl PersistentRecord for PersistentReceiveValue {
    type Domain = Zatoshis;

    fn from_domain(domain: &Zatoshis) -> Self {
        Self(u64::from(*domain))
    }

    fn into_domain(self) -> Result<Zatoshis, DecodeError> {
        Zatoshis::new(self.0).map_err(|e| DecodeError::Invalid(e.to_string()))
    }
}

/// A read of the address-history index failed.
#[derive(Debug, thiserror::Error)]
pub enum ReceivesReadError {
    /// The backend read failed.
    #[error(transparent)]
    Backend(#[from] ReadError),
    /// A persisted entry could not be decoded.
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

/// Read all receives for `addr`, height-ordered — the read side of this index.
///
/// Decodes back exactly what the engine wrote via this index's [`Schema`]. It
/// scans the namespace and filters by the address prefix; a prefix range scan is
/// a future backend optimisation (the key is address-prefixed precisely to
/// enable it, so this fn's contract does not change when it lands).
pub fn read_receives(
    reader: &dyn BackendReader,
    addr: AddrId,
) -> Result<Vec<AddressReceive>, ReceivesReadError> {
    let mut out = Vec::new();
    for (raw_key, raw_value) in reader.scan(ID.into())? {
        let key = decode_key::<AddressHistoryIndex>(&raw_key)?;
        if key.addr == addr {
            out.push(AddressReceive {
                addr: key.addr,
                height: key.height,
                txid: key.txid,
                output_index: key.output_index,
                value: decode_value::<AddressHistoryIndex>(&raw_value)?,
            });
        }
    }
    out.sort_by_key(|r| (r.height.value(), r.output_index));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_persistence_codec::{encode_key, encode_value};

    fn txid(b: u8) -> TransactionId {
        TransactionId::from([b; 32])
    }

    /// A P2PKH script locking to `hash` (0x76 0xa9 0x14 <20> 0x88 0xac).
    fn p2pkh(hash: [u8; 20]) -> Script {
        let mut s = vec![0x76, 0xa9, 0x14];
        s.extend_from_slice(&hash);
        s.extend_from_slice(&[0x88, 0xac]);
        Script::from(s)
    }

    #[test]
    fn extract_records_a_receive_per_output() {
        let ctx = AddressCtx {
            height: BlockHeight::new(100),
            txs: vec![(
                txid(1),
                vec![
                    (0, Zatoshis::new(500).expect("valid"), p2pkh([7; 20])),
                    (1, Zatoshis::new(300).expect("valid"), p2pkh([9; 20])),
                ],
            )],
        };
        let receives = AddressHistoryIndex::extract(&ctx).expect("extract");
        assert_eq!(receives.len(), 2);
        assert_eq!(receives[0].addr.hash, [7; 20]);
        assert_eq!(receives[0].output_index, 0);
        assert_eq!(receives[1].addr.hash, [9; 20]);
        assert_eq!(receives[1].output_index, 1);
    }

    #[test]
    fn key_round_trips_and_is_address_prefixed() {
        let key = AddrKey {
            addr: AddrId {
                script_type: ScriptType::P2PKH,
                hash: [7; 20],
            },
            height: BlockHeight::new(100),
            txid: txid(1),
            output_index: 2,
        };
        let bytes = encode_key::<AddressHistoryIndex>(&key);
        assert_eq!(bytes.len(), 65);
        // Address id is the leading 21 bytes — the range-scan prefix.
        assert_eq!(bytes[0], 0); // P2PKH
        assert_eq!(&bytes[1..21], &[7u8; 20]);
        assert_eq!(
            decode_key::<AddressHistoryIndex>(&bytes).expect("decode"),
            key
        );
    }

    #[test]
    fn value_round_trips() {
        let bytes = encode_value::<AddressHistoryIndex>(&Zatoshis::new(12345).expect("valid"));
        assert_eq!(
            decode_value::<AddressHistoryIndex>(&bytes).expect("decode"),
            Zatoshis::new(12345).expect("valid")
        );
    }

    #[test]
    fn value_encodes_to_a_pinned_8_byte_layout() {
        // Golden vector: 12345 = 0x3039, little-endian over 8 bytes.
        let bytes = encode_value::<AddressHistoryIndex>(&Zatoshis::new(12345).expect("valid"));
        assert_eq!(bytes, vec![0x39, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            decode_value::<AddressHistoryIndex>(&bytes).expect("decode"),
            Zatoshis::new(12345).expect("valid")
        );
    }

    #[test]
    fn key_encodes_to_a_pinned_65_byte_layout() {
        // Golden vector: type(1) ++ hash(20) ++ height(8 BE) ++ txid(32) ++ index(4 BE).
        let key = AddrKey {
            addr: AddrId {
                script_type: ScriptType::P2SH,
                hash: [0xAB; 20],
            },
            height: BlockHeight::new(0x0102),
            txid: txid(0xCD),
            output_index: 0x0304,
        };
        let mut expected = Vec::new();
        expected.push(1u8); // P2SH
        expected.extend_from_slice(&[0xAB; 20]);
        expected.extend_from_slice(&0x0102u64.to_be_bytes());
        expected.extend_from_slice(&[0xCD; 32]);
        expected.extend_from_slice(&0x0304u32.to_be_bytes());
        assert_eq!(expected.len(), 65);

        let bytes = encode_key::<AddressHistoryIndex>(&key);
        assert_eq!(bytes, expected);
        assert_eq!(
            decode_key::<AddressHistoryIndex>(&bytes).expect("decode"),
            key
        );
    }

    #[test]
    fn receives_round_trip_through_the_backend() {
        use zaino_persistence::in_memory::InMemoryBackend;
        use zaino_persistence::{Backend, BackendWriter, WriteOp};

        let addr_a = AddrId {
            script_type: ScriptType::P2PKH,
            hash: [7; 20],
        };
        let addr_b = AddrId {
            script_type: ScriptType::P2PKH,
            hash: [9; 20],
        };
        let z = |n| Zatoshis::new(n).expect("valid");
        let receives = vec![
            AddressReceive {
                addr: addr_a,
                height: BlockHeight::new(10),
                txid: txid(1),
                output_index: 0,
                value: z(500),
            },
            AddressReceive {
                addr: addr_b,
                height: BlockHeight::new(11),
                txid: txid(2),
                output_index: 0,
                value: z(300),
            },
            AddressReceive {
                addr: addr_a,
                height: BlockHeight::new(12),
                txid: txid(3),
                output_index: 1,
                value: z(700),
            },
        ];

        // Write exactly as the engine's persist step does: into_entries -> encode
        // -> Put. So this reads back what the running indexer would have written.
        let ops: Vec<WriteOp> = AddressHistoryIndex::into_entries(vec![receives])
            .into_iter()
            .map(|(k, v)| WriteOp::Put {
                namespace: ID.into(),
                key: encode_key::<AddressHistoryIndex>(&k),
                value: encode_value::<AddressHistoryIndex>(&v),
            })
            .collect();
        let backend = InMemoryBackend::new();
        let mut writer = backend.writer().expect("writer");
        writer.commit(ops).expect("commit");
        let reader = backend.reader().expect("reader");

        // addr_a: two receives, height-ordered (10 then 12).
        let a = read_receives(&reader, addr_a).expect("read a");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].height, BlockHeight::new(10));
        assert_eq!(a[0].value, z(500));
        assert_eq!(a[1].height, BlockHeight::new(12));
        assert_eq!(a[1].output_index, 1);

        // addr_b: one receive; a different address: none.
        assert_eq!(read_receives(&reader, addr_b).expect("read b").len(), 1);
        let addr_c = AddrId {
            script_type: ScriptType::P2SH,
            hash: [1; 20],
        };
        assert!(read_receives(&reader, addr_c).expect("read c").is_empty());
    }
}
