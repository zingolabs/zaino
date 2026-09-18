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

use zaino_persistence_codec::{DecodeError, EntryCodec};
use zaino_primitives::types::{
    classify_script, OutputIndex, Script, ScriptType, TransactionId, Zatoshis,
};
use zaino_sync::backend::{BackendReader, ReadError};
use zaino_sync::descriptor::{Append, BlockLocal};
use zaino_sync::primitives::{BlockHeight, IndexId};
use zaino_sync::traits::{ExtractError, ExtractLocal, IndexDef, MergeAppend, Schema};

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
    fn extract(ctx: &AddressCtx) -> Result<Self::Delta, ExtractError> {
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

    fn encode_key(key: &AddrKey) -> Vec<u8> {
        let mut buf = Vec::with_capacity(65);
        buf.push(script_type_byte(key.addr.script_type));
        buf.extend_from_slice(&key.addr.hash);
        buf.extend_from_slice(&key.height.value().to_be_bytes());
        buf.extend_from_slice(&<[u8; 32]>::from(key.txid));
        buf.extend_from_slice(&key.output_index.to_be_bytes());
        buf
    }

    fn encode_value(value: &Zatoshis) -> Vec<u8> {
        u64::from(*value).to_le_bytes().to_vec()
    }

    fn decode_key(bytes: &[u8]) -> Result<AddrKey, DecodeError> {
        if bytes.len() != 65 {
            return Err(DecodeError::Invalid(format!(
                "expected 65 bytes, got {}",
                bytes.len()
            )));
        }
        let script_type = script_type_from_byte(bytes[0])?;
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&bytes[1..21]);
        let height = u64::from_be_bytes(bytes[21..29].try_into().expect("8 bytes"));
        let mut txid = [0u8; 32];
        txid.copy_from_slice(&bytes[29..61]);
        let output_index = u32::from_be_bytes(bytes[61..65].try_into().expect("4 bytes"));
        Ok(AddrKey {
            addr: AddrId { script_type, hash },
            height: BlockHeight::new(height),
            txid: TransactionId::from(txid),
            output_index,
        })
    }

    fn decode_value(bytes: &[u8]) -> Result<Zatoshis, DecodeError> {
        if bytes.len() != 8 {
            return Err(DecodeError::Invalid(format!(
                "expected 8 bytes, got {}",
                bytes.len()
            )));
        }
        let raw = u64::from_le_bytes(bytes.try_into().expect("8 bytes"));
        Zatoshis::new(raw).map_err(|e| DecodeError::Invalid(e.to_string()))
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
        let key = AddressHistoryIndex::decode_key(&raw_key)?;
        if key.addr == addr {
            out.push(AddressReceive {
                addr: key.addr,
                height: key.height,
                txid: key.txid,
                output_index: key.output_index,
                value: AddressHistoryIndex::decode_value(&raw_value)?,
            });
        }
    }
    out.sort_by_key(|r| (r.height.value(), r.output_index));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let bytes = AddressHistoryIndex::encode_key(&key);
        assert_eq!(bytes.len(), 65);
        // Address id is the leading 21 bytes — the range-scan prefix.
        assert_eq!(bytes[0], 0); // P2PKH
        assert_eq!(&bytes[1..21], &[7u8; 20]);
        assert_eq!(
            AddressHistoryIndex::decode_key(&bytes).expect("decode"),
            key
        );
    }

    #[test]
    fn value_round_trips() {
        let bytes = AddressHistoryIndex::encode_value(&Zatoshis::new(12345).expect("valid"));
        assert_eq!(
            AddressHistoryIndex::decode_value(&bytes).expect("decode"),
            Zatoshis::new(12345).expect("valid")
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
                key: AddressHistoryIndex::encode_key(&k),
                value: AddressHistoryIndex::encode_value(&v),
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
