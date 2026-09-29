//! Deterministic block records, encoded through the production codecs.
//!
//! A backend benchmark is only honest if the bytes it stores are the bytes the
//! indexer stores. So nothing here invents a record layout: each entry goes
//! through [`zaino_persistence_codec::put`] with the real index's
//! [`EntryCodec`](zaino_persistence_codec::EntryCodec), which encodes it via the
//! same on-disk DTO the engine writes. Key and value *sizes* therefore follow
//! the real format automatically, and a format change moves the benchmark with
//! it rather than leaving it measuring a fiction.
//!
//! What the generator does choose is the **shape** of a block: how many
//! transactions, and how many outputs, spends and actions each carries. Those
//! are what make a mainnet block 1 KiB or 1 MiB, and they are a [`Shape`] knob
//! rather than a constant, because the eras differ by more than an order of
//! magnitude (see the sandblast band in the sync-bench reports).
//!
//! Randomness is a local SplitMix64 rather than a `rand` dependency: the stream
//! is then pinned by this file alone, so a dependency bump cannot silently
//! change the input two backends are compared on.

use zaino_indexes::indexes::chain_metadata::ChainMetadataIndex;
use zaino_indexes::indexes::hash_to_height::HashToHeightIndex;
use zaino_indexes::indexes::headers::{HeaderValue, HeadersIndex};
use zaino_indexes::indexes::ironwood::{IronwoodBlockValue, IronwoodIndex};
use zaino_indexes::indexes::orchard::{OrchardBlockValue, OrchardIndex, OrchardTxCompact};
use zaino_indexes::indexes::sapling::{SaplingBlockValue, SaplingIndex, SaplingTxCompact};
use zaino_indexes::indexes::transparent_data::{
    TransparentBlockValue, TransparentDataIndex, TransparentTxCompact,
};
use zaino_indexes::indexes::txids::{TxidsIndex, TxidsValue};
use zaino_indexes::indexes::{
    chain_metadata, hash_to_height, headers, ironwood, orchard, sapling, transparent_data, txids,
};
use zaino_persistence::{Namespace, WriteOp};
use zaino_persistence_codec::{encode_key, put, watermark};
use zaino_primitives::types::{
    BlockHash, ChainMetadata, CompactCiphertext, CompactDifficulty, EphemeralKey, Height,
    NoteCommitment, Nullifier, Script, TransactionId, TreeSize, Zatoshis,
};
use zaino_sync::primitives::BlockHeight;

use crate::error::BenchError;

/// How much shielded and transparent material a generated block carries.
///
/// The three settings bracket what mainnet actually holds, so a run says which
/// era it is measuring rather than reporting one unlabelled number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// An early-chain block: a couple of transparent transactions, no shielded
    /// material to speak of. Small records, so the backend's per-key overhead
    /// dominates.
    Light,
    /// A mid-chain block: a handful of transactions across all pools. The
    /// default, and the shape most of the chain resembles.
    Typical,
    /// A 2022-spam-era block: output-heavy shielded transactions. Large records,
    /// so value bytes dominate and write amplification shows.
    Sandblast,
}

impl Shape {
    /// A stable lowercase tag for the result schema.
    pub fn as_str(self) -> &'static str {
        match self {
            Shape::Light => "light",
            Shape::Typical => "typical",
            Shape::Sandblast => "sandblast",
        }
    }

    /// The per-block counts this shape generates.
    fn counts(self) -> Counts {
        match self {
            Shape::Light => Counts {
                transactions: 2,
                transparent_outputs: 2,
                sapling_outputs: 0,
                orchard_actions: 0,
                ironwood_actions: 0,
            },
            Shape::Typical => Counts {
                transactions: 8,
                transparent_outputs: 2,
                sapling_outputs: 2,
                orchard_actions: 2,
                ironwood_actions: 0,
            },
            Shape::Sandblast => Counts {
                transactions: 24,
                transparent_outputs: 1,
                sapling_outputs: 24,
                orchard_actions: 2,
                ironwood_actions: 0,
            },
        }
    }
}

/// Per-block item counts, which set the record sizes.
#[derive(Debug, Clone, Copy)]
struct Counts {
    transactions: usize,
    transparent_outputs: usize,
    sapling_outputs: usize,
    orchard_actions: usize,
    ironwood_actions: usize,
}

/// The namespaces a compact-block deployment writes per block, in the order the
/// store reads them back. Exposed so the read workloads address exactly the
/// namespaces the write workload filled.
pub fn block_namespaces() -> [Namespace; 7] {
    [
        headers::ID.into(),
        chain_metadata::ID.into(),
        txids::ID.into(),
        transparent_data::ID.into(),
        sapling::ID.into(),
        orchard::ID.into(),
        ironwood::ID.into(),
    ]
}

/// The namespace mapping a block hash to its height — the extra lookup a read
/// by hash pays before the fan-out.
pub fn hash_namespace() -> Namespace {
    hash_to_height::ID.into()
}

/// One block's worth of write operations, plus what the read workloads need to
/// address them.
pub struct BlockRecords {
    /// The entries, encoded through the production codecs.
    pub ops: Vec<WriteOp>,
    /// The encoded height key every per-height namespace shares.
    pub height_key: Vec<u8>,
    /// The encoded hash key of the hash→height entry.
    pub hash_key: Vec<u8>,
    /// Key and value bytes of this block's header entry, for `--verify`.
    pub header_entry: (Vec<u8>, Vec<u8>),
    /// Logical bytes this block contributes: every key and value written.
    pub bytes: u64,
}

/// Deterministic record generator.
///
/// Seeded once per run, so two backends receive an identical byte stream and the
/// comparison is of the storage, not of the input.
pub struct Generator {
    state: u64,
    counts: Counts,
}

impl Generator {
    /// A generator for `shape`, seeded by `seed`.
    pub fn new(shape: Shape, seed: u64) -> Self {
        Self {
            state: seed,
            counts: shape.counts(),
        }
    }

    /// SplitMix64: one multiply-xor round per draw, period 2^64, and no
    /// dependency whose version could change the stream.
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A pseudo-random 32-byte array — a hash, nullifier, commitment or key.
    fn bytes32(&mut self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for chunk in out.chunks_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes()[..chunk.len()]);
        }
        out
    }

    /// The 52-byte compact ciphertext head a light client scans.
    fn ciphertext(&mut self) -> Result<CompactCiphertext, BenchError> {
        let mut out = [0u8; CompactCiphertext::LENGTH];
        for chunk in out.chunks_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes()[..chunk.len()]);
        }
        CompactCiphertext::try_new(&out).map_err(|e| BenchError::Generate(e.to_string()))
    }

    /// Generate the records for one height.
    pub fn block(&mut self, height: Height) -> Result<BlockRecords, BenchError> {
        let key = BlockHeight::new(u64::from(u32::from(height)));
        let hash = BlockHash::from(self.bytes32());
        let counts = self.counts;

        let header = HeaderValue {
            hash,
            prev_hash: BlockHash::from(self.bytes32()),
            time: u32::try_from(self.next_u64() % u64::from(u32::MAX))
                .map_err(|e| BenchError::Generate(e.to_string()))?,
            bits: CompactDifficulty::try_from_bits(0x1d00_ffff)
                .map_err(|e| BenchError::Generate(e.to_string()))?,
        };

        let txids_value = TxidsValue(
            (0..counts.transactions)
                .map(|_| TransactionId::from(self.bytes32()))
                .collect(),
        );

        let mut transparent = Vec::with_capacity(counts.transactions);
        for _ in 0..counts.transactions {
            let mut outputs = Vec::with_capacity(counts.transparent_outputs);
            for _ in 0..counts.transparent_outputs {
                let value = Zatoshis::new(self.next_u64() % 1_000_000_000)
                    .map_err(|e| BenchError::Generate(e.to_string()))?;
                // A p2pkh lock script: the 25-byte form most outputs carry.
                let mut script = vec![0x76, 0xa9, 0x14];
                script.extend_from_slice(&self.bytes32()[..20]);
                script.extend_from_slice(&[0x88, 0xac]);
                outputs.push((value, Script::new(script)));
            }
            transparent.push(TransparentTxCompact {
                inputs: vec![(TransactionId::from(self.bytes32()), 0)],
                outputs,
            });
        }

        let mut sapling = Vec::with_capacity(counts.transactions);
        for _ in 0..counts.transactions {
            let mut outputs = Vec::with_capacity(counts.sapling_outputs);
            for _ in 0..counts.sapling_outputs {
                outputs.push((
                    NoteCommitment::from(self.bytes32()),
                    EphemeralKey::from(self.bytes32()),
                    self.ciphertext()?,
                ));
            }
            sapling.push(SaplingTxCompact {
                nullifiers: vec![Nullifier::from(self.bytes32())],
                outputs,
            });
        }

        let orchard_value = OrchardBlockValue(self.actions(counts.orchard_actions)?);
        let ironwood_value = IronwoodBlockValue(self.actions(counts.ironwood_actions)?);

        let tree_size = TreeSize::from(u32::from(height));
        let metadata = ChainMetadata {
            sapling_tree_size: tree_size,
            orchard_tree_size: tree_size,
            ironwood_tree_size: tree_size,
        };

        let ops = vec![
            put::<HeadersIndex>(headers::ID.into(), &key, &header),
            put::<ChainMetadataIndex>(chain_metadata::ID.into(), &key, &metadata),
            put::<TxidsIndex>(txids::ID.into(), &key, &txids_value),
            put::<TransparentDataIndex>(
                transparent_data::ID.into(),
                &key,
                &TransparentBlockValue(transparent),
            ),
            put::<SaplingIndex>(sapling::ID.into(), &key, &SaplingBlockValue(sapling)),
            put::<OrchardIndex>(orchard::ID.into(), &key, &orchard_value),
            put::<IronwoodIndex>(ironwood::ID.into(), &key, &ironwood_value),
            put::<HashToHeightIndex>(hash_to_height::ID.into(), &hash, &key),
        ];

        let bytes = ops.iter().map(op_bytes).sum();
        let header_entry = (
            encode_key::<HeadersIndex>(&key),
            zaino_persistence_codec::encode_value::<HeadersIndex>(&header),
        );

        Ok(BlockRecords {
            ops,
            height_key: encode_key::<HeadersIndex>(&key),
            hash_key: encode_key::<HashToHeightIndex>(&hash),
            header_entry,
            bytes,
        })
    }

    /// Orchard-shaped actions, shared by the Orchard and Ironwood pools.
    fn actions(&mut self, per_tx: usize) -> Result<Vec<OrchardTxCompact>, BenchError> {
        let mut txs = Vec::with_capacity(self.counts.transactions);
        for _ in 0..self.counts.transactions {
            let mut actions = Vec::with_capacity(per_tx);
            for _ in 0..per_tx {
                actions.push((
                    Nullifier::from(self.bytes32()),
                    NoteCommitment::from(self.bytes32()),
                    EphemeralKey::from(self.bytes32()),
                    self.ciphertext()?,
                ));
            }
            txs.push(OrchardTxCompact { actions });
        }
        Ok(txs)
    }
}

/// The watermark operation the engine includes in every batch, so a benchmark
/// batch commits exactly what a real one does.
pub fn watermark_op(height: Height) -> WriteOp {
    watermark::stamp(height)
}

/// Key plus value bytes of one operation — the logical volume, before any
/// backend's own overhead.
fn op_bytes(op: &WriteOp) -> u64 {
    let len = match op {
        WriteOp::Put { key, value, .. } => key.len().saturating_add(value.len()),
        WriteOp::Delete { key, .. } => key.len(),
    };
    u64::try_from(len).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(value: u32) -> Height {
        Height::try_from(value).expect("valid test height")
    }

    #[test]
    fn the_same_seed_generates_the_same_bytes() {
        let first = Generator::new(Shape::Typical, 7)
            .block(height(1))
            .expect("generates");
        let second = Generator::new(Shape::Typical, 7)
            .block(height(1))
            .expect("generates");

        assert_eq!(first.bytes, second.bytes);
        assert_eq!(first.header_entry, second.header_entry);
        assert_eq!(first.hash_key, second.hash_key);
    }

    #[test]
    fn a_different_seed_generates_different_bytes() {
        let first = Generator::new(Shape::Typical, 7)
            .block(height(1))
            .expect("generates");
        let other = Generator::new(Shape::Typical, 8)
            .block(height(1))
            .expect("generates");

        assert_ne!(first.header_entry, other.header_entry);
    }

    /// Every namespace the store reads on a compose must be written, or the
    /// read workloads would measure misses.
    #[test]
    fn a_block_writes_every_namespace_the_store_composes_from() {
        let records = Generator::new(Shape::Typical, 1)
            .block(height(100))
            .expect("generates");

        let written: Vec<Namespace> = records
            .ops
            .iter()
            .map(|op| match op {
                WriteOp::Put { namespace, .. } | WriteOp::Delete { namespace, .. } => *namespace,
            })
            .collect();

        for namespace in block_namespaces() {
            assert!(written.contains(&namespace), "{namespace} missing");
        }
        assert!(written.contains(&hash_namespace()), "hash→height missing");
    }

    /// The shapes must actually differ in volume, or a run's label means
    /// nothing.
    #[test]
    fn heavier_shapes_write_more_bytes_per_block() {
        let light = Generator::new(Shape::Light, 1)
            .block(height(1))
            .expect("generates")
            .bytes;
        let typical = Generator::new(Shape::Typical, 1)
            .block(height(1))
            .expect("generates")
            .bytes;
        let sandblast = Generator::new(Shape::Sandblast, 1)
            .block(height(1))
            .expect("generates")
            .bytes;

        assert!(light < typical, "{light} < {typical}");
        assert!(typical < sandblast, "{typical} < {sandblast}");
    }
}
