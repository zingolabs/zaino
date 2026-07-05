//! Guard: one rustls CryptoProvider — aws-lc-rs — plus the single tolerated
//! ring path through zebra (ADR-0006).
//!
//! Two checks against the production workspace's resolved dependency graph:
//!
//! 1. rustls must resolve with `aws_lc_rs` and `prefer-post-quantum`.
//! 2. The set of feature edges that select ring anywhere in the graph must
//!    equal [`RING_EDGE_ALLOWLIST`] — today, the single chain rooted in
//!    zebra-node-services' `rpc-client` feature (its reqwest `rustls-tls`;
//!    reqwest 0.12 offers no aws-lc alternative). That ring is compiled but
//!    dormant: reqwest consults the process-default provider first and zaino
//!    installs aws-lc-rs at both TLS boundaries.
//!
//! Loud in both directions. A NEW edge means a second provider path is
//! creeping back in via feature unification — the regression this guard
//! exists to catch (zingolabs/zaino#1360). A MISSING edge — in particular
//! ring vanishing from the graph entirely — means zebra dropped its ring
//! dependence: delete [`RING_EDGE_ALLOWLIST`], make this check assert ring's
//! absence, and close the classical-deprecation tracking issue's zebra item.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use workbench::{repo_root, run};

/// Feature names that select the ring provider when enabled on any crate.
const RING_SELECTING_FEATURES: [&str; 3] = ["ring", "__rustls-ring", "tls-ring"];

/// Every feature edge expected to select ring, all downstream of
/// zebra-node-services' reqwest `rustls-tls` (see module docs).
const RING_EDGE_ALLOWLIST: [&str; 5] = [
    "hyper-rustls feature \"ring\"",
    "reqwest feature \"__rustls-ring\"",
    "rustls feature \"ring\"",
    "rustls-webpki feature \"ring\"",
    "tokio-rustls feature \"ring\"",
];

fn main() {
    run("check-crypto-provider", check, |()| {
        println!(
            "check-crypto-provider: ok — rustls resolves aws-lc-rs + prefer-post-quantum; \
             ring edges match the tolerated zebra path"
        );
    })
}

fn check() -> Result<(), Vec<String>> {
    let root = repo_root()?;

    let rustls_features = cargo_tree(&root, &["--package", "rustls", "--depth", "0"], "{f}")?;
    check_preferred_provider(&rustls_features)?;

    let ring_tree = match cargo_tree(&root, &["--invert", "ring", "--edges", "features"], "{p}") {
        Ok(out) => out,
        Err(diag)
            if diag
                .iter()
                .any(|l| l.contains("did not match any packages")) =>
        {
            return Err(vec![
                "ring is GONE from the dependency graph — zebra dropped it!".to_string(),
                "Tighten this guard: delete RING_EDGE_ALLOWLIST and assert ring's absence,"
                    .to_string(),
                "and close the zebra item on the classical-deprecation tracking issue.".to_string(),
            ]);
        }
        Err(diag) => return Err(diag),
    };
    check_ring_edges(&ring_tree)
}

/// Assert the resolved rustls feature set contains the preferred-provider pair.
fn check_preferred_provider(features_line: &str) -> Result<(), Vec<String>> {
    let features: BTreeSet<&str> = features_line.trim().split(',').collect();
    let missing: Vec<&str> = ["aws_lc_rs", "prefer-post-quantum"]
        .into_iter()
        .filter(|f| !features.contains(f))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(vec![format!(
            "rustls resolved without {} (got: {}) — the preferred provider per \
             ADR-0006 is aws-lc-rs with prefer-post-quantum; check \
             packages/zaino-common/Cargo.toml's rustls features",
            missing.join(" + "),
            features_line.trim(),
        )])
    }
}

/// Assert the ring-selecting feature edges equal the tolerated allowlist.
fn check_ring_edges(ring_tree: &str) -> Result<(), Vec<String>> {
    let found = ring_selecting_edges(ring_tree);
    let expected: BTreeSet<String> = RING_EDGE_ALLOWLIST
        .iter()
        .map(ToString::to_string)
        .collect();

    if found == expected {
        return Ok(());
    }

    let mut msg = Vec::new();
    for new in found.difference(&expected) {
        msg.push(format!(
            "NEW ring-selecting feature edge: {new} — a second CryptoProvider path is \
             creeping back in (zingolabs/zaino#1360); remove the enabling feature",
        ));
    }
    for gone in expected.difference(&found) {
        msg.push(format!(
            "tolerated ring edge disappeared: {gone} — if zebra dropped ring, tighten \
             this guard (see the module docs in check-crypto-provider.rs)",
        ));
    }
    msg.push("see docs/adr/0006-aws-lc-rs-preferred-crypto-provider.md".to_string());
    Err(msg)
}

/// The deduplicated `<crate> feature "<name>"` edges in a
/// `cargo tree --edges features` output whose feature name selects ring.
fn ring_selecting_edges(tree: &str) -> BTreeSet<String> {
    tree.lines()
        .map(|line| {
            line.trim_start_matches(['│', '├', '└', '─', ' '])
                .trim_end_matches(" (*)")
        })
        .filter(|node| {
            node.split_once(" feature ").is_some_and(|(_, feature)| {
                RING_SELECTING_FEATURES
                    .iter()
                    .any(|f| feature == format!("\"{f}\""))
            })
        })
        .map(ToString::to_string)
        .collect()
}

/// Run `cargo tree <args> --format <format>` against the production workspace
/// at `root`, returning stdout; diagnostics carry stderr so callers can
/// distinguish "package not in graph" from other failures.
fn cargo_tree(root: &Path, args: &[&str], format: &str) -> Result<String, Vec<String>> {
    let output = Command::new("cargo")
        .arg("tree")
        .args(args)
        .args(["--format", format])
        .current_dir(root)
        .output()
        .map_err(|e| vec![format!("failed to run cargo tree: {e}")])?;
    if !output.status.success() {
        return Err(vec![format!(
            "`cargo tree {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim(),
        )]);
    }
    String::from_utf8(output.stdout).map_err(|e| vec![format!("cargo output not utf-8: {e}")])
}

#[cfg(test)]
mod ring_selecting_edges {
    use super::*;

    #[test]
    fn extracts_and_dedups_ring_edges_from_a_feature_tree() {
        let tree = "\
ring v0.17.14
├── rustls v0.23.41
│   ├── rustls feature \"ring\"
│   │   └── reqwest feature \"__rustls-ring\"
│   └── rustls feature \"ring\" (*)
└── rustls-webpki v0.103.13
    └── rustls-webpki feature \"ring\"
";
        let edges = ring_selecting_edges(tree);
        assert_eq!(
            edges.into_iter().collect::<Vec<_>>(),
            [
                "reqwest feature \"__rustls-ring\"",
                "rustls feature \"ring\"",
                "rustls-webpki feature \"ring\"",
            ]
        );
    }

    #[test]
    fn ignores_package_nodes_and_non_ring_features() {
        let tree = "\
rustls v0.23.41
├── rustls feature \"aws_lc_rs\"
├── reqwest feature \"rustls-tls\"
└── tokio-rustls feature \"logging\"
";
        assert!(ring_selecting_edges(tree).is_empty());
    }

    #[test]
    fn matches_feature_names_exactly_not_by_substring() {
        // "tls-ring-extra" must not match the "tls-ring" selector.
        let tree = "└── tonic feature \"tls-ring-extra\"\n";
        assert!(ring_selecting_edges(tree).is_empty());
    }
}
