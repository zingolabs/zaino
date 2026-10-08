//! The real sync engine builds the `tree_state` index byte-for-byte equal to a
//! sequential `Frontier::append`, across batch boundaries, on both backends, and
//! across a resume from a mid-chain watermark.
//!
//! The fixtures are `CurrentZainoContext` blocks with staggered pool activity
//! (Sapling from genesis, Orchard from height 5, Ironwood from height 15, plus
//! zero-commitment blocks), so the ordered-monoid scan is exercised over empty
//! pools, single-leaf blocks and multi-leaf blocks, and the per-pool start
//! positions the measure prefix-sums must get right. Every stored per-height
//! frontier is compared against an independent sequential fold built with
//! `incrementalmerkletree`'s own `Frontier::append`.

use incrementalmerkletree::frontier::Frontier;
use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node as SaplingNode;

use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
use zaino_indexes::indexes::orchard::OrchardTxCompact;
use zaino_indexes::indexes::sapling::SaplingTxCompact;
use zaino_indexes::indexes::tree_state::codec::TreeStateIndex;
use zaino_indexes::indexes::tree_state::pools::{orchard_leaf, sapling_leaf};
use zaino_indexes::indexes::tree_state::segment::DEPTH;
use zaino_indexes::indexes::tree_state::TreeStateValue;
use zaino_indexes::sets::current_zaino::CurrentZainoContext;
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, BackendReader, Namespace, NamespaceSpec};
use zaino_persistence_codec::{decode_value, reserved_namespaces};
use zaino_primitives::types::{
    BlockHash, CompactCiphertext, CompactDifficulty, EphemeralKey, NoteCommitment, Nullifier,
};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::index_pipelines::IndexPipelines;
use zaino_sync::primitives::BlockHeight;

const CHAIN_LEN: u64 = 30;
const BATCH: u32 = 7;

/// A canonical note commitment (both Jubjub and Pallas accept this encoding):
/// the seed in the low 8 bytes, the rest zero.
fn commitment(seed: u64) -> NoteCommitment {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    NoteCommitment::from(bytes)
}

/// How many commitments each pool adds at `height`. Sapling is active from
/// genesis, Orchard from height 5, Ironwood from height 15; the counts vary
/// (including zero) so the scan sees ragged, empty and multi-leaf blocks.
fn counts(height: u64) -> (usize, usize, usize) {
    let sapling = usize::try_from((height * 7 + 1) % 5).expect("fits");
    let orchard = if height >= 5 {
        usize::try_from((height * 3) % 4).expect("fits")
    } else {
        0
    };
    let ironwood = if height >= 15 {
        usize::try_from((height * 2 + 1) % 3).expect("fits")
    } else {
        0
    };
    (sapling, orchard, ironwood)
}

/// The whole test chain as `CurrentZainoContext` blocks, each pool's commitments
/// drawn from a disjoint, chain-wide-distinct seed range so every leaf is unique
/// (an order bug then shows as a wrong root).
fn chain() -> Vec<CurrentZainoContext> {
    let epk = EphemeralKey::from([0u8; 32]);
    let ciphertext = CompactCiphertext::from([0u8; CompactCiphertext::LENGTH]);
    let bits = CompactDifficulty::try_from_bits(0x1d00_ffff).expect("valid nBits");

    let nullifier = Nullifier::from([0u8; 32]);
    let mut sapling_seed = 0u64;
    let mut orchard_seed = 1_000_000u64;
    let mut ironwood_seed = 2_000_000u64;

    (0..CHAIN_LEN)
        .map(|height| {
            let (s, o, i) = counts(height);

            let sapling_outputs = (0..s)
                .map(|_| {
                    let cmu = commitment(sapling_seed);
                    sapling_seed += 1;
                    (cmu, epk, ciphertext)
                })
                .collect();
            let orchard_actions = (0..o)
                .map(|_| {
                    let cmx = commitment(orchard_seed);
                    orchard_seed += 1;
                    (nullifier, cmx, epk, ciphertext)
                })
                .collect();
            let ironwood_actions = (0..i)
                .map(|_| {
                    let cmx = commitment(ironwood_seed);
                    ironwood_seed += 1;
                    (nullifier, cmx, epk, ciphertext)
                })
                .collect();

            CurrentZainoContext {
                height: BlockHeight::new(height),
                hash: BlockHash::ZERO,
                prev_hash: BlockHash::ZERO,
                time: 0,
                bits,
                txids: Vec::new(),
                spends: Vec::new(),
                txid_locations: Vec::new(),
                transparent_txs: Vec::new(),
                sapling_txs: vec![SaplingTxCompact {
                    nullifiers: Vec::new(),
                    outputs: sapling_outputs,
                }],
                orchard_txs: vec![OrchardTxCompact {
                    actions: orchard_actions,
                }],
                ironwood_txs: vec![OrchardTxCompact {
                    actions: ironwood_actions,
                }],
            }
        })
        .collect()
}

/// Append one pool's leaves to a frontier, panicking only on a depth overflow
/// (never for this chain).
fn append<H: Hashable + Clone>(frontier: &mut Frontier<H, DEPTH>, leaves: &[H]) {
    for leaf in leaves {
        assert!(frontier.append(leaf.clone()), "append within depth");
    }
}

/// The reference per-height `TreeStateValue` series: fold each pool's leaves
/// with `incrementalmerkletree`'s own `Frontier::append`, snapshotting after
/// each block.
fn reference_series(blocks: &[CurrentZainoContext]) -> Vec<(u64, TreeStateValue)> {
    let mut sapling = Frontier::<SaplingNode, DEPTH>::empty();
    let mut orchard = Frontier::<MerkleHashOrchard, DEPTH>::empty();
    let mut ironwood = Frontier::<MerkleHashOrchard, DEPTH>::empty();
    blocks
        .iter()
        .map(|block| {
            let s: Vec<SaplingNode> = block
                .sapling_txs
                .iter()
                .flat_map(|tx| tx.outputs.iter())
                .map(|(cmu, _, _)| sapling_leaf((*cmu).into()).expect("canonical cmu"))
                .collect();
            let o: Vec<MerkleHashOrchard> = block
                .orchard_txs
                .iter()
                .flat_map(|tx| tx.actions.iter())
                .map(|(_, cmx, _, _)| orchard_leaf((*cmx).into()).expect("canonical cmx"))
                .collect();
            let i: Vec<MerkleHashOrchard> = block
                .ironwood_txs
                .iter()
                .flat_map(|tx| tx.actions.iter())
                .map(|(_, cmx, _, _)| orchard_leaf((*cmx).into()).expect("canonical cmx"))
                .collect();
            append(&mut sapling, &s);
            append(&mut orchard, &o);
            append(&mut ironwood, &i);
            (
                u64::from(block.height),
                TreeStateValue {
                    sapling: sapling.clone(),
                    orchard: orchard.clone(),
                    ironwood: ironwood.clone(),
                },
            )
        })
        .collect()
}

/// The stored `tree_state` series read back from a backend, as `(height,
/// value)`, ascending by height.
fn stored_series<B: Backend>(backend: &B) -> Vec<(u64, TreeStateValue)> {
    let namespace = Namespace::new("tree_state");
    let reader = backend.reader().expect("reader");
    reader
        .scan(namespace)
        .expect("scan tree_state")
        .into_iter()
        .map(|(key, value)| {
            let height = u64::from_be_bytes(key.as_slice().try_into().expect("8-byte height key"));
            let decoded = decode_value::<TreeStateIndex>(&value).expect("tree-state value decodes");
            (height, decoded)
        })
        .collect()
}

fn pipelines() -> IndexPipelines<CurrentZainoContext> {
    IndexPipelines::new().with::<TreeStateIndex>()
}

fn engine<B: Backend>(backend: B, start: u64) -> SyncEngine<CurrentZainoContext, B> {
    SyncEngine::from_pipelines(
        pipelines(),
        backend,
        EngineConfig {
            batch_size: BATCH,
            start_height: BlockHeight::new(start),
        },
    )
    .expect("engine builds")
}

/// The engine's declared namespaces plus the reserved meta namespaces it stamps,
/// for a backend that must open its namespaces upfront (LMDB).
fn specs() -> Vec<NamespaceSpec> {
    pipelines()
        .namespace_specs()
        .into_iter()
        .chain(reserved_namespaces().map(NamespaceSpec::meta))
        .collect()
}

// (a) In-memory backend, 30 blocks in batches of 7: every stored frontier equals
// the sequential fold.
#[test]
fn in_memory_build_equals_sequential_append() {
    let blocks = chain();
    let expected = reference_series(&blocks);

    let backend = InMemoryBackend::new();
    let mut eng = engine(backend.clone(), 0);
    eng.sync_range(blocks).expect("sync succeeds");

    assert_eq!(stored_series(&backend), expected);
}

// (b) The same over an LMDB temp-dir backend.
#[test]
fn lmdb_build_equals_sequential_append() {
    let blocks = chain();
    let expected = reference_series(&blocks);

    let tmp = tempfile::tempdir().expect("tempdir");
    let backend = LmdbBackend::open(LmdbConfig {
        path: tmp.path().to_path_buf(),
        map_size_bytes: 64 << 20,
        namespaces: specs(),
    })
    .expect("open lmdb");

    let mut eng = engine(backend.clone(), 0);
    eng.sync_range(blocks).expect("sync succeeds");

    assert_eq!(stored_series(&backend), expected);
}

// (c) Resume from a mid-chain watermark equals an uninterrupted build: stop after
// the blocks below the split, then a fresh engine on the same backend continues
// from the split (reloading the carry by point-reading the frontier at the
// watermark).
#[test]
fn resume_from_watermark_equals_uninterrupted_build() {
    let blocks = chain();
    let expected = reference_series(&blocks);
    let split = 13u64;

    let backend = InMemoryBackend::new();

    // Phase 1: heights [0, split).
    {
        let mut eng = engine(backend.clone(), 0);
        eng.sync_range(
            blocks
                .iter()
                .take(usize::try_from(split).expect("fits"))
                .cloned()
                .collect(),
        )
        .expect("phase 1 sync");
    }
    let watermark = SyncEngine::<CurrentZainoContext, _>::committed_height(&backend)
        .expect("read watermark")
        .expect("watermark exists");
    assert_eq!(watermark, BlockHeight::new(split - 1));

    // Phase 2: a fresh engine continues from the split.
    {
        let mut eng = engine(backend.clone(), split);
        eng.sync_range(
            blocks
                .into_iter()
                .skip(usize::try_from(split).expect("fits"))
                .collect(),
        )
        .expect("phase 2 sync");
    }

    assert_eq!(stored_series(&backend), expected);
}
