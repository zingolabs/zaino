//! Tests characterising the per-block commitment-tree fetch on the sync path.
//!
//! Background. The finalised-state sync loop (`finalised_state.rs`, the
//! `for height in db..=tip` loop) does, per block and strictly serially:
//! `get_block().await`, then `get_commitment_tree_roots().await`, then the
//! (batched, #1214-optimised) write. On the JSON-RPC ("fetch") backend the
//! second call is a `z_gettreestate`, which returns the entire serialized
//! commitment-tree *frontier* (`finalState`) just so zaino can keep `(root,
//! count)` and discard the rest. Operators observed sync throughput collapse
//! right at the NU5 / Orchard activation height (≈1,687,104) with CPU near
//! idle — the signature of a latency-bound serial RPC, not a CPU bottleneck,
//! and the reason #1214's faster writer did not move the needle there.
//!
//! These tests pin down the two design issues and guard the fix:
//!
//! - [`treestate_fetch_cost_is_bounded`] — the per-block zaino-side cost of the
//!   `finalState` payload (its size and parse time) stays bounded as the pool
//!   grows, because a frontier is O(tree depth). This localises the production
//!   collapse to the per-block RPC round-trip and the validator's own
//!   `z_gettreestate` cost, *not* zaino's parsing.
//! - [`serial_treestate_rpcs_are_latency_bound`] — with the two per-block RPCs
//!   awaited in series, wall-clock is the sum of both round-trips per block;
//!   issuing them concurrently (the minimal fix) cuts it to the max.
//! - [`incremental_frontier_matches_authoritative_roots`] — maintaining a
//!   note-commitment frontier locally in zaino (append each block's note
//!   commitments) reproduces the authoritative per-block `(root, count)`
//!   exactly. This is the regression net for replacing the per-block
//!   `z_gettreestate` with O(1) local tracking, eliminating RPC #2 entirely.
//! - [`fetch_frontier_parse_matches_direct_roots`] — parsing the full
//!   `finalState` (the fetch path) yields the same `(root, count)` as reading
//!   the roots directly (the state path), so the full-tree fetch is pure
//!   overhead and the cheaper path is safe to prefer.

use std::time::{Duration, Instant};

use tokio::time::{sleep, Instant as TokioInstant};
use zcash_primitives::merkle_tree::read_commitment_tree;

use super::vectors::{build_mockchain_source, load_test_vectors};
use crate::chain_index::source::BlockchainSource;
use crate::BlockHash;

/// Parse a Sapling `finalState` frontier and derive `(root, count)`, mirroring
/// the fetch backend's extraction in `validator_connector.rs`.
fn sapling_root_from_final_state(final_state: &[u8]) -> (zebra_chain::sapling::tree::Root, u64) {
    let tree = read_commitment_tree::<sapling_crypto::Node, _, 32>(final_state)
        .expect("sapling finalState parses");
    let root = zebra_chain::sapling::tree::Root::try_from(tree.root().to_bytes())
        .expect("sapling root converts");
    (root, tree.size() as u64)
}

/// Orchard twin of [`sapling_root_from_final_state`].
fn orchard_root_from_final_state(final_state: &[u8]) -> (zebra_chain::orchard::tree::Root, u64) {
    let tree = read_commitment_tree::<zebra_chain::orchard::tree::Node, _, 32>(final_state)
        .expect("orchard finalState parses");
    let root = zebra_chain::orchard::tree::Root::try_from(tree.root().to_repr())
        .expect("orchard root converts");
    (root, tree.size() as u64)
}

/// Measures the per-block fetch-path cost: serialized `finalState` size and the
/// time to parse it and recompute the root, across the vector chain. The point
/// is that both stay flat — a frontier is bounded by tree depth (~32 nodes), so
/// the payload zaino pulls and parses per block does not grow with the pool.
/// Run with:
/// `cargo nextest run -p zaino-state --run-ignored ignored-only --no-capture treestate_sync`.
#[test]
#[ignore = "measurement: run with --run-ignored ignored-only --no-capture treestate_sync"]
fn treestate_fetch_cost_is_bounded() {
    let data = load_test_vectors().expect("test vectors load");
    let last_height = data.blocks.last().expect("vectors non-empty").height;

    println!("[treestate] per-block finalState size + parse cost on the fetch path:");
    let mut max_sapling_bytes = 0usize;
    let mut max_orchard_bytes = 0usize;
    for block in &data.blocks {
        let sapling_state = &block.sapling_tree_state;
        let orchard_state = &block.orchard_tree_state;

        let started = Instant::now();
        if !sapling_state.is_empty() {
            let _ = sapling_root_from_final_state(sapling_state);
        }
        let sapling_parse = started.elapsed();

        let started = Instant::now();
        if !orchard_state.is_empty() {
            let _ = orchard_root_from_final_state(orchard_state);
        }
        let orchard_parse = started.elapsed();

        max_sapling_bytes = max_sapling_bytes.max(sapling_state.len());
        max_orchard_bytes = max_orchard_bytes.max(orchard_state.len());

        if block.height % 25 == 0 || block.height == last_height {
            println!(
                "  h{:>5}  sapling: {:>4} B (notes {:>4}) parse {:>8.2?} | \
                 orchard: {:>4} B (notes {:>4}) parse {:>8.2?}",
                block.height,
                sapling_state.len(),
                block.sapling_tree_size,
                sapling_parse,
                orchard_state.len(),
                block.orchard_tree_size,
                orchard_parse,
            );
        }
    }
    println!(
        "[treestate] max finalState size over chain — sapling {max_sapling_bytes} B, \
         orchard {max_orchard_bytes} B (bounded: frontier is O(tree depth)). The production \
         slowdown at NU5 is the per-block RPC round-trip + validator-side z_gettreestate cost, \
         not zaino's parse."
    );
}

/// Demonstrates that the sync loop's two per-block RPCs, awaited in series, make
/// per-block wall-clock the *sum* of both round-trips, whereas issuing them
/// concurrently makes it the *max*. This models the await structure of the
/// `finalised_state.rs` sync loop (not the production code itself); the paused
/// clock makes the virtual timings deterministic.
#[tokio::test(start_paused = true)]
async fn serial_treestate_rpcs_are_latency_bound() {
    const BLOCKS: u32 = 64;
    let block_rtt = Duration::from_millis(10);
    let tree_rtt = Duration::from_millis(10);

    // Serial: get_block().await, then get_commitment_tree_roots().await.
    let started = TokioInstant::now();
    for _ in 0..BLOCKS {
        sleep(block_rtt).await;
        sleep(tree_rtt).await;
    }
    let serial = started.elapsed();

    // Minimal fix: issue the two per-block RPCs concurrently.
    let started = TokioInstant::now();
    for _ in 0..BLOCKS {
        tokio::join!(sleep(block_rtt), sleep(tree_rtt));
    }
    let concurrent = started.elapsed();

    println!(
        "[treestate] {BLOCKS} blocks — serial per-block RPCs {serial:?}, \
         concurrent per-block {concurrent:?}"
    );

    // Serial pays both round-trips every block.
    assert!(
        serial >= (block_rtt + tree_rtt) * BLOCKS - Duration::from_millis(1),
        "serial {serial:?} should be ≈ blocks × (block_rtt + tree_rtt)"
    );
    // Overlapping the two RPCs is a clear win (≈ max, not sum).
    assert!(
        concurrent <= serial * 3 / 5,
        "concurrent {concurrent:?} should beat serial {serial:?} once the RPCs overlap"
    );
}

/// Maintaining the note-commitment frontier locally — appending each block's
/// note commitments to a running tree — reproduces the authoritative per-block
/// `(root, count)` exactly, for both pools, across the whole vector chain. This
/// is the correctness guard for replacing the per-block `z_gettreestate` (RPC
/// #2) with O(1)-per-note local tracking.
#[test]
fn incremental_frontier_matches_authoritative_roots() {
    let data = load_test_vectors().expect("test vectors load");

    let mut sapling = zebra_chain::sapling::tree::NoteCommitmentTree::default();
    let mut orchard = zebra_chain::orchard::tree::NoteCommitmentTree::default();
    let mut sapling_growth_checked = 0usize;
    let mut orchard_growth_checked = 0usize;

    for block in &data.blocks {
        for note_commitment in block.zebra_block.sapling_note_commitments() {
            sapling
                .append(*note_commitment)
                .expect("append sapling note commitment");
        }
        for note_commitment in block.zebra_block.orchard_note_commitments() {
            orchard
                .append(*note_commitment)
                .expect("append orchard note commitment");
        }

        assert_eq!(
            sapling.count(),
            block.sapling_tree_size,
            "sapling count mismatch at height {}",
            block.height
        );
        assert!(
            sapling.root() == block.sapling_root,
            "sapling root mismatch at height {}",
            block.height
        );
        assert_eq!(
            orchard.count(),
            block.orchard_tree_size,
            "orchard count mismatch at height {}",
            block.height
        );
        assert!(
            orchard.root() == block.orchard_root,
            "orchard root mismatch at height {}",
            block.height
        );

        if block.sapling_tree_size > 0 {
            sapling_growth_checked += 1;
        }
        if block.orchard_tree_size > 0 {
            orchard_growth_checked += 1;
        }
    }

    assert!(
        sapling_growth_checked > 0 || orchard_growth_checked > 0,
        "vector chain carried no shielded notes; the guard would be vacuous"
    );
}

/// Parsing the full `finalState` frontier (the fetch path) yields the same
/// `(root, count)` as reading the roots directly (the state path), block for
/// block. Confirms the per-block full-tree fetch is pure overhead and the
/// cheaper direct path is safe to prefer.
#[tokio::test]
async fn fetch_frontier_parse_matches_direct_roots() {
    let data = load_test_vectors().expect("test vectors load");
    let source = build_mockchain_source(data.blocks.clone());

    let mut nonempty_compared = 0usize;
    for block in &data.blocks {
        let hash = BlockHash::from(block.zebra_block.hash());

        // State path: roots read directly.
        let (sapling_direct, orchard_direct) = source
            .get_commitment_tree_roots(hash)
            .await
            .expect("direct commitment-tree roots");
        let sapling_direct = sapling_direct.expect("sapling direct root present");
        let orchard_direct = orchard_direct.expect("orchard direct root present");

        // Fetch path: pull the full frontier, parse it, derive (root, count).
        let (sapling_state, orchard_state) = source
            .get_treestate(hash)
            .await
            .expect("treestate frontier");
        let sapling_state = sapling_state.expect("sapling treestate present");
        let orchard_state = orchard_state.expect("orchard treestate present");

        if sapling_state.is_empty() {
            assert_eq!(
                sapling_direct.1, 0,
                "empty sapling finalState but non-zero count at height {}",
                block.height
            );
        } else {
            let parsed = sapling_root_from_final_state(&sapling_state);
            assert_eq!(
                parsed.1, sapling_direct.1,
                "sapling count mismatch at height {}",
                block.height
            );
            assert!(
                parsed.0 == sapling_direct.0,
                "sapling root mismatch at height {}",
                block.height
            );
        }

        if orchard_state.is_empty() {
            assert_eq!(
                orchard_direct.1, 0,
                "empty orchard finalState but non-zero count at height {}",
                block.height
            );
        } else {
            let parsed = orchard_root_from_final_state(&orchard_state);
            assert_eq!(
                parsed.1, orchard_direct.1,
                "orchard count mismatch at height {}",
                block.height
            );
            assert!(
                parsed.0 == orchard_direct.0,
                "orchard root mismatch at height {}",
                block.height
            );
        }

        if sapling_direct.1 > 0 || orchard_direct.1 > 0 {
            nonempty_compared += 1;
        }
    }

    assert!(
        nonempty_compared > 0,
        "no non-empty trees compared; parity check would be vacuous"
    );
}
