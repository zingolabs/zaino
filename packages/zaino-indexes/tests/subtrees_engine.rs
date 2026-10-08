//! The real sync engine builds the subtree-roots cross index from `tree_state`:
//! each completed subtree's stored root equals an independent perfect-subtree
//! computation, and the completing height matches the block that crossed the
//! boundary.
//!
//! A subtree completes only every `2^16` leaves, which is impractical to drive
//! synthetically, so a test pool lowers the subtree level to 2 (span 4). The
//! chain is laid out so subtrees complete mid-block, two in one block, and at a
//! block boundary, with the first subtree's left half straddling the carried
//! (pre-block) `tree_state` frontier. A small batch size puts the dependency
//! predecessor both in the same batch (read through the pending overlay) and in
//! an earlier committed batch, so the cross index's `DepsReader` path is
//! exercised both ways.

use incrementalmerkletree::frontier::Frontier;
use incrementalmerkletree::{Hashable, Level};
use sapling_crypto::Node as SaplingNode;

use zaino_indexes::indexes::sapling::{SaplingIndex, SaplingTxCompact};
use zaino_indexes::indexes::subtrees::codec::SubtreeRoot;
use zaino_indexes::indexes::subtrees::pool::Pool;
use zaino_indexes::indexes::subtrees::SubtreesIndex;
use zaino_indexes::indexes::tree_state::codec::TreeStateIndex;
use zaino_indexes::indexes::tree_state::pools::sapling_leaf;
use zaino_indexes::indexes::tree_state::segment::DEPTH;
use zaino_indexes::indexes::tree_state::{TreeStateCtx, TreeStateValue};
use zaino_indexes::sets::current_zaino::CurrentZainoContext;
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, BackendReader, Namespace};
use zaino_persistence_codec::decode_value;
use zaino_primitives::types::{
    BlockHash, CompactCiphertext, CompactDifficulty, EphemeralKey, NoteCommitment, Nullifier,
};
use zaino_sync::engine::{EngineConfig, SyncEngine};
use zaino_sync::index_pipelines::IndexPipelines;
use zaino_sync::primitives::{BlockHeight, IndexId};

/// A test pool mirroring Sapling but with a subtree level of 2, so a handful of
/// leaves completes several subtrees.
struct TestPool;

impl Pool for TestPool {
    type Leaf = SaplingNode;

    const NAME: IndexId = IndexId::new("subtrees_test");
    const COMPACT: IndexId = zaino_indexes::indexes::sapling::ID;
    const POOL: &'static str = "sapling-test";
    const SUBTREE_LEVEL: u8 = 2;

    fn commitments(ctx: &TreeStateCtx) -> &[NoteCommitment] {
        &ctx.sapling_cmus
    }

    fn leaf(bytes: [u8; 32]) -> Option<SaplingNode> {
        sapling_leaf(bytes)
    }

    fn frontier(value: &TreeStateValue) -> &Frontier<SaplingNode, DEPTH> {
        &value.sapling
    }

    fn root_bytes(node: &SaplingNode) -> [u8; 32] {
        node.to_bytes()
    }
}

type TestSubtrees = SubtreesIndex<TestPool>;

const LEVEL: u8 = 2;

/// A canonical Sapling note commitment with its seed in the low 8 bytes.
fn commitment(seed: u64) -> NoteCommitment {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    NoteCommitment::from(bytes)
}

/// The leaf a commitment seed maps to.
fn leaf(seed: u64) -> SaplingNode {
    sapling_leaf(commitment(seed).into()).expect("canonical cmu")
}

/// The independent perfect-subtree root over exactly `2^LEVEL` leaves.
fn perfect_root(leaves: &[SaplingNode]) -> SaplingNode {
    assert_eq!(leaves.len(), 1usize << LEVEL);
    let mut nodes = leaves.to_vec();
    for l in 0..LEVEL {
        nodes = nodes
            .chunks_exact(2)
            .map(|pair| SaplingNode::combine(Level::from(l), &pair[0], &pair[1]))
            .collect();
    }
    nodes.into_iter().next().expect("one root")
}

/// One block carrying `count` sapling outputs, consuming seeds from `next`.
fn block(height: u64, count: u64, next: &mut u64) -> CurrentZainoContext {
    let epk = EphemeralKey::from([0u8; 32]);
    let ciphertext = CompactCiphertext::from([0u8; CompactCiphertext::LENGTH]);
    let bits = CompactDifficulty::try_from_bits(0x1d00_ffff).expect("valid nBits");

    let outputs = (0..count)
        .map(|_| {
            let cmu = commitment(*next);
            *next += 1;
            (cmu, epk, ciphertext)
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
            nullifiers: vec![Nullifier::from([0u8; 32])],
            outputs,
        }],
        orchard_txs: Vec::new(),
        ironwood_txs: Vec::new(),
    }
}

#[test]
fn engine_builds_subtree_roots_from_tree_state() {
    // Sizes: h0→3, h1→9 (completes subtrees 0 and 1), h2→12 (completes subtree 2
    // at the block boundary). 12 sapling leaves total.
    let mut seed = 0u64;
    let blocks = vec![
        block(0, 3, &mut seed),
        block(1, 6, &mut seed),
        block(2, 3, &mut seed),
    ];
    let all: Vec<SaplingNode> = (0..12).map(leaf).collect();

    let backend = InMemoryBackend::new();
    let pipelines = IndexPipelines::new()
        .with::<SaplingIndex>()
        .with::<TreeStateIndex>()
        .with::<TestSubtrees>();
    let mut engine = SyncEngine::from_pipelines(
        pipelines,
        backend.clone(),
        EngineConfig {
            // Batch 2: predecessor of h1 is in-batch (overlay), of h2 committed.
            batch_size: 2,
            start_height: BlockHeight::new(0),
        },
    )
    .expect("engine builds");
    engine.sync_range(blocks).expect("sync succeeds");

    // Read the subtree namespace back, ascending by key.
    let namespace = Namespace::new("subtrees_test");
    let reader = backend.reader().expect("reader");
    let stored: Vec<(u32, SubtreeRoot)> = reader
        .scan(namespace)
        .expect("scan subtrees_test")
        .into_iter()
        .map(|(key, value)| {
            let index = u32::from_be_bytes(key.as_slice().try_into().expect("4-byte subtree key"));
            let root = decode_value::<TestSubtrees>(&value).expect("value decodes");
            (index, root)
        })
        .collect();

    let expected = vec![
        (
            0u32,
            SubtreeRoot {
                root: perfect_root(&all[0..4]).to_bytes(),
                completing_height: BlockHeight::new(1),
            },
        ),
        (
            1,
            SubtreeRoot {
                root: perfect_root(&all[4..8]).to_bytes(),
                completing_height: BlockHeight::new(1),
            },
        ),
        (
            2,
            SubtreeRoot {
                root: perfect_root(&all[8..12]).to_bytes(),
                completing_height: BlockHeight::new(2),
            },
        ),
    ];
    assert_eq!(stored, expected);
}
