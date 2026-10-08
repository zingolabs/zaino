//! Golden tests for the legacy commitment-tree wire encoding against zebra.
//!
//! The fixtures are real `z_gettreestate` responses captured from zebra 6.4.2
//! (lazy fork, mainnet) at a spread of heights straddling Sapling and Orchard
//! activation. For every pool with a `finalState`:
//!
//! 1. the `finalState` (zcashd legacy `CommitmentTree` hex) decodes into a
//!    frontier and re-encodes **byte-identically** through `legacy_tree_bytes`
//!    (the decode/encode pair is the wire contract), and
//! 2. the frontier's root equals the fixture's `finalRoot`.
//!
//! Root byte orientation (verified here against the fixtures): zebra reports
//! `z_gettreestate`'s `finalRoot` the same way block headers do — **Sapling
//! reversed** (display order, the reverse of a node's `to_bytes()`) and
//! **Orchard in internal order** (as `to_bytes()` yields). The `exactly one
//! orientation matches` check plus the per-pool assertions below pin this and
//! fail loudly if a future capture changes convention.

use incrementalmerkletree::frontier::Frontier;
use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node as SaplingNode;

use zaino_indexes::indexes::tree_state::{legacy_tree_bytes, legacy_tree_from_bytes};

const FIXTURE: &str = include_str!("fixtures/treestate/zebra-mainnet.json");

/// Which byte orientation of a node's `to_bytes()` matched the fixture root.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Orientation {
    Direct,
    Reversed,
}

/// Decode a pool's `finalState`, assert the re-encode is byte-identical, and
/// return the orientation in which the frontier root matches `finalRoot`.
fn check_pool<H>(final_state_hex: &str, final_root_hex: &str) -> Orientation
where
    H: zcash_primitives::merkle_tree::HashSer + incrementalmerkletree::Hashable + Clone + NodeBytes,
{
    let state_bytes = hex::decode(final_state_hex).expect("finalState is hex");
    let frontier: Frontier<H, 32> =
        legacy_tree_from_bytes(&state_bytes).expect("finalState decodes to a frontier");

    // (1) byte-identical round trip through the legacy encoder.
    let reencoded = legacy_tree_bytes(&frontier);
    assert_eq!(
        hex::encode(&reencoded),
        final_state_hex,
        "legacy_tree_bytes must reproduce the zebra finalState exactly"
    );

    // (2) the root matches finalRoot in exactly one orientation.
    let root_bytes = frontier.root().node_bytes();
    let final_root = hex::decode(final_root_hex).expect("finalRoot is hex");
    let mut reversed = root_bytes.to_vec();
    reversed.reverse();

    let direct = root_bytes.as_slice() == final_root.as_slice();
    let rev = reversed.as_slice() == final_root.as_slice();
    assert!(
        direct ^ rev,
        "root must match finalRoot in exactly one orientation (direct={direct}, reversed={rev})"
    );
    if direct {
        Orientation::Direct
    } else {
        Orientation::Reversed
    }
}

/// Access a node's canonical 32-byte representation uniformly across pools.
trait NodeBytes {
    fn node_bytes(&self) -> [u8; 32];
}
impl NodeBytes for SaplingNode {
    fn node_bytes(&self) -> [u8; 32] {
        self.to_bytes()
    }
}
impl NodeBytes for MerkleHashOrchard {
    fn node_bytes(&self) -> [u8; 32] {
        self.to_bytes()
    }
}

#[test]
fn legacy_final_state_round_trips_and_root_matches_zebra() {
    let fixture: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    let heights = fixture["z_gettreestate"]
        .as_object()
        .expect("z_gettreestate object");

    let mut sapling_orientations = Vec::new();
    let mut orchard_orientations = Vec::new();
    let mut checked = 0usize;

    for (height, response) in heights {
        let result = &response["result"];
        for pool in ["sapling", "orchard"] {
            let commitments = &result[pool]["commitments"];
            let (Some(final_state), Some(final_root)) = (
                commitments["finalState"].as_str(),
                commitments["finalRoot"].as_str(),
            ) else {
                // Below the pool's activation height: `commitments` is `{}`.
                continue;
            };
            let orientation = match pool {
                "sapling" => check_pool::<SaplingNode>(final_state, final_root),
                _ => check_pool::<MerkleHashOrchard>(final_state, final_root),
            };
            match pool {
                "sapling" => sapling_orientations.push((height.clone(), orientation)),
                _ => orchard_orientations.push((height.clone(), orientation)),
            }
            checked += 1;
        }
    }

    assert!(
        checked >= 8,
        "expected several pool states, checked {checked}"
    );
    // Each pool uses one consistent orientation across every height, and that
    // orientation is pinned to what zebra reports (see this file's header).
    let expected = [
        ("sapling", &sapling_orientations, Orientation::Reversed),
        ("orchard", &orchard_orientations, Orientation::Direct),
    ];
    for (pool, seen, want) in expected {
        if let Some(((_, first), rest)) = seen.split_first() {
            assert!(
                rest.iter().all(|(_, o)| o == first),
                "{pool} root orientation must be consistent across heights: {seen:?}"
            );
            assert_eq!(*first, want, "{pool} finalRoot orientation changed");
        }
    }
}
