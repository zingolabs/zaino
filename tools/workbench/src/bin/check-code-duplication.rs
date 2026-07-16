//! Guard: no duplicate Rust logic may enter the tree.
//!
//! Runs cargo-dupes' AST-normalizing analyzer (identifiers become positional
//! placeholders and literals are erased, so renamed copies still match) over the
//! first-party Rust scopes and fails if any exact or near duplicate group remains:
//!
//! - `packages/` — the production crates, excluding `zaino-proto` (tonic/prost
//!   generated code) and in-crate `tests/` module directories.
//! - `live-tests/` — the e2e and clientless harness crates, including their
//!   integration-test helper code.
//! - `tools/` — the developer tooling.
//!
//! `#[test]` functions and `#[cfg(test)]` modules visible per-file are excluded in
//! every scope. The tree was deduplicated to zero groups when this gate landed, so
//! both thresholds are zero: any new group is a regression.
//!
//! `MIN_NODES` was calibrated empirically against this tree on 2026-07-15: 50 is
//! the floor at which every reported group was a genuine dedup-worthy twin. Below
//! it the analyzer reports structurally-rhyming but semantically unrelated small
//! functions (field-copy constructors, tiny delegating getters), whose "dedup"
//! would couple unrelated types.
//!
//! Escape hatch: a genuinely irreducible group may be granted an annotated entry
//! in `.dupes-ignore.toml` at the repo root — run
//! `code-dupes ignore <fingerprint> --reason "..."` (crates.io package
//! `code-dupes`, the CLI over the same libraries). Prefer deduplicating; entries
//! are reviewed like code.

use std::path::{Path, PathBuf};

use dupes_core::config::Config;
use dupes_core::scanner::{scan_files, ScanConfig};
use dupes_rust::RustAnalyzer;
use workbench::{repo_root, run};

/// One scanned scope: a repo-relative directory and its excluded path substrings
/// (matched against repo-relative paths, so the checkout location cannot affect
/// the result).
struct Scope {
    path: &'static str,
    excludes: &'static [&'static str],
}

const SCOPES: &[Scope] = &[
    Scope {
        path: "packages",
        excludes: &["zaino-proto", "/tests/"],
    },
    Scope {
        path: "live-tests",
        excludes: &[],
    },
    Scope {
        path: "tools",
        excludes: &[],
    },
];

/// Minimum AST-node count for a code unit to participate in duplicate analysis.
/// See the module docs for the calibration rationale.
const MIN_NODES: usize = 50;

fn main() {
    run("check-code-duplication", check, |n| {
        println!("check-code-duplication: ok — no duplicate groups across {n} Rust file(s)");
    })
}

fn check() -> Result<usize, Vec<String>> {
    let root = repo_root()?;
    let analyzer = RustAnalyzer;
    let mut scanned_files = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    for scope in SCOPES {
        let files: Vec<PathBuf> = scan_files(&ScanConfig::new(root.join(scope.path)))
            .into_iter()
            .filter(|path| !is_excluded(path, &root, scope.excludes))
            .collect();
        scanned_files += files.len();

        let config = Config {
            min_nodes: MIN_NODES,
            exclude_tests: true,
            // The ignore file (`.dupes-ignore.toml`) is discovered at this root.
            root: root.clone(),
            ..Config::default()
        };

        let result = dupes_core::analyze(&analyzer, &files, &config)
            .map_err(|e| vec![format!("analysis of {} failed: {e}", scope.path)])?;
        for warning in &result.warnings {
            eprintln!(
                "check-code-duplication: warning ({}): {warning}",
                scope.path
            );
        }

        for (label, groups) in [
            ("exact", &result.exact_groups),
            ("near", &result.near_groups),
        ] {
            for group in groups {
                offenders.push(format!(
                    "{label} duplicate group in {} (fingerprint {}, similarity {:.0}%):",
                    scope.path,
                    group.fingerprint,
                    group.similarity * 100.0
                ));
                for member in &group.members {
                    let file = member.file.strip_prefix(&root).unwrap_or(&member.file);
                    offenders.push(format!(
                        "  - {} ({}) at {}:{}-{}",
                        member.name,
                        member.kind,
                        file.display(),
                        member.line_start,
                        member.line_end
                    ));
                }
            }
        }
    }

    if offenders.is_empty() {
        return Ok(scanned_files);
    }
    let mut msg = vec![
        "duplicate Rust logic found — deduplicate it (prefer a plain fn; see CLAUDE.md §DRY):"
            .to_string(),
    ];
    msg.extend(offenders);
    msg.push(
        "if a group is genuinely irreducible, add an annotated entry to .dupes-ignore.toml \
         (`code-dupes ignore <fingerprint> --reason \"...\"`)."
            .to_string(),
    );
    Err(msg)
}

/// Whether `path`, taken relative to the repo root, matches any exclude substring.
fn is_excluded(path: &Path, root: &Path, excludes: &[&str]) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    excludes.iter().any(|pattern| relative.contains(pattern))
}
