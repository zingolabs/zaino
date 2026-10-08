//! Golden cross-check of the first Sapling and Orchard subtree roots against
//! zebra's `z_getsubtreesbyindex` fixtures.
//!
//! Computing these roots requires the first `3 · 2^16` note-commitment leaves of
//! each pool, which are mainnet data far too large to commit to the repo. So the
//! leaf-driven equality is `#[ignore]`d here and validated in the Task 10 cluster
//! run (a full mainnet build with zebra cross-checks). This file pins the fixture
//! shape the cluster run compares against, and records the byte orientation:
//! subtree roots are reported — and stored by this crate — in internal
//! (unreversed) order for both pools, unlike `z_gettreestate`'s Sapling
//! `finalRoot`.

use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/treestate/zebra-mainnet.json");

/// The `(root hex, end_height)` of each subtree in a pool's fixture response.
fn pool_subtrees(fixture: &Value, pool: &str) -> Vec<(String, u64)> {
    fixture["z_getsubtreesbyindex"][pool]["result"]["subtrees"]
        .as_array()
        .expect("subtrees array")
        .iter()
        .map(|subtree| {
            (
                subtree["root"].as_str().expect("root hex").to_owned(),
                subtree["end_height"].as_u64().expect("end_height"),
            )
        })
        .collect()
}

/// The fixtures hold three subtree roots per pool, each a 32-byte hex root with
/// an ascending end height — the shape the Task 10 cluster run compares the
/// locally built roots against.
#[test]
fn fixture_subtree_roots_are_well_formed() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    for pool in ["sapling", "orchard"] {
        let subtrees = pool_subtrees(&fixture, pool);
        assert_eq!(subtrees.len(), 3, "{pool}: three fixture subtrees");
        let mut last_height = 0u64;
        for (root_hex, end_height) in subtrees {
            let root = hex::decode(&root_hex).expect("root is hex");
            assert_eq!(root.len(), 32, "{pool}: a subtree root is 32 bytes");
            assert!(
                end_height > last_height,
                "{pool}: subtree end heights ascend"
            );
            last_height = end_height;
        }
    }
}

/// Leaf-driven cross-check: the first three Sapling / Orchard subtree roots this
/// crate builds equal the zebra fixtures. Requires the first `3 · 2^16` leaves of
/// each pool (mainnet data, not in the repo), so it is validated in the Task 10
/// cluster build rather than here.
#[test]
#[ignore = "needs 3·2^16 mainnet leaves per pool; validated in the Task 10 cluster run"]
fn built_subtree_roots_match_zebra() {
    // Intentionally empty: the equality is asserted against a full mainnet build
    // with zebra cross-checks in Task 10. See the module docs.
}
