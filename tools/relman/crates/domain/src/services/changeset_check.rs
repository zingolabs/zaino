use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use relman_config::ReleaseConfig;
use relman_core::ports::{ChangesetCheck, ChangesetStore, CheckError, CheckReport, Vcs, Violation};
use relman_core::types::{Changeset, ChangesetError, CrateName, Slug, StoredChangeset};

/// The changeset-file extension. Only `*.toml` under the changesets dir count as
/// this-PR changeset files.
const TOML_EXT: &str = "toml";

/// Enforces the `dev`-gate rule: a PR touching a governed target's source must
/// carry a covering changeset **in its own diff**. Implements the
/// [`ChangesetCheck`] driving port over the [`Vcs`] and [`ChangesetStore`]
/// driven ports and the loaded [`ReleaseConfig`].
///
/// The load-bearing distinction is *this-PR* vs *accumulated*: only changeset
/// files that appear in the PR's diff (`vcs.changed_files`) count toward
/// coverage. An accumulated changeset left in `.changesets/` by a
/// previously-merged PR is in the store but not in the diff, so it never
/// satisfies this PR's touch of a target.
pub struct ChangesetCheckService {
    config: ReleaseConfig,
    vcs: Arc<dyn Vcs>,
    store: Arc<dyn ChangesetStore>,
}

impl ChangesetCheckService {
    pub fn new(config: ReleaseConfig, vcs: Arc<dyn Vcs>, store: Arc<dyn ChangesetStore>) -> Self {
        Self { config, vcs, store }
    }

    /// The slug of a changed path iff it is a this-PR changeset file: a
    /// `*.toml` under the configured changesets dir whose stem is a valid slug.
    fn changeset_slug(&self, file: &Path, changesets_dir: &Path) -> Option<Slug> {
        if !file.starts_with(changesets_dir) {
            return None;
        }
        if file.extension().and_then(|e| e.to_str()) != Some(TOML_EXT) {
            return None;
        }
        let stem = file.file_stem().and_then(|s| s.to_str())?;
        Slug::parse(stem).ok()
    }

    /// The set of targets whose source `files` touch, sorted by name for
    /// deterministic violation ordering.
    fn touched_targets(&self, files: &[PathBuf]) -> Vec<CrateName> {
        let mut touched: Vec<CrateName> = files
            .iter()
            .filter_map(|file| self.config.target_owning_path(file))
            .map(|target| target.name().clone())
            .collect();
        touched.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        touched.dedup();
        touched
    }
}

impl ChangesetCheck for ChangesetCheckService {
    fn check(&self, base: &str) -> Result<CheckReport, CheckError> {
        let changed = self.vcs.changed_files(base)?;
        let changesets_dir = self.config.options().changesets_dir().as_path();

        // Partition the diff into this-PR changeset files (by slug) and
        // everything else (candidate source files).
        let mut pr_changeset_slugs: Vec<Slug> = Vec::new();
        let mut source_files: Vec<PathBuf> = Vec::new();
        for file in changed {
            match self.changeset_slug(&file, changesets_dir) {
                Some(slug) => pr_changeset_slugs.push(slug),
                None => source_files.push(file),
            }
        }

        let touched = self.touched_targets(&source_files);

        // Read every this-PR changeset, collecting coverage, the waiver flag,
        // and any parse / unknown-target violations.
        let mut violations: Vec<Violation> = Vec::new();
        let mut covered: HashSet<CrateName> = HashSet::new();
        let mut waiver = false;
        for slug in &pr_changeset_slugs {
            let raw = self.store.read(slug)?;
            let stored = match StoredChangeset::parse_toml(&raw) {
                Ok(stored) => stored,
                // An unfilled template covers nothing, but it is not malformed:
                // flag it as its own violation so the author gets a targeted
                // "fill this in" message rather than a generic parse error. The
                // target it should have covered stays uncovered below.
                Err(ChangesetError::Unfilled) => {
                    violations.push(Violation::UnfilledTemplate(
                        changesets_dir.join(slug.file_name()),
                    ));
                    continue;
                }
                Err(error) => {
                    violations.push(Violation::ChangesetParse {
                        file: changesets_dir.join(slug.file_name()),
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            // A changeset already consumed by a past release is historical
            // provenance: it neither covers this PR's touched targets nor waives
            // the PR. Skip it, exactly as the derivation does.
            if stored.consumed_in().is_some() {
                continue;
            }
            match stored.into_body() {
                Changeset::Empty { .. } => waiver = true,
                Changeset::WithChanges(entries) => {
                    for entry in entries {
                        let name = entry.crate_name();
                        if self.config.target_by_name(name).is_some() {
                            covered.insert(name.clone());
                        } else {
                            violations.push(Violation::UnknownTargetInChangeset(
                                name.as_str().to_owned(),
                            ));
                        }
                    }
                }
            }
        }

        // Decide coverage.
        if !touched.is_empty() && pr_changeset_slugs.is_empty() {
            violations.push(Violation::NoChangesetForTouchedTargets);
        } else if waiver {
            // An empty changeset waives the whole PR; parse/unknown violations
            // (if any) were already surfaced above.
        } else {
            for target in &touched {
                if !covered.contains(target) {
                    violations.push(Violation::TargetUncovered(target.clone()));
                }
            }
        }

        Ok(CheckReport { violations })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use relman_config::ReleaseConfig;
    use relman_core::mocks::{MapChangesetStore, StubVcs};
    use relman_core::ports::ChangesetStore;
    use relman_core::types::{CrateName, ReleaseOptions, Target, WorkspacePath};

    fn path(raw: &str) -> WorkspacePath {
        WorkspacePath::parse(raw).expect("valid workspace path")
    }

    fn crate_name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    /// A config with two governed targets, A (`zaino-state`) and B (`zainod`),
    /// and the default `.changesets` dir.
    fn config() -> ReleaseConfig {
        let options = ReleaseOptions::new(
            path(".changesets"),
            path("Cargo.toml"),
            path("CHANGELOG.md"),
        );
        let targets = vec![
            Target::new(
                crate_name("zaino-state"),
                path("packages/zaino-state"),
                path("packages/zaino-state/CHANGELOG.md"),
                true,
            ),
            Target::new(
                crate_name("zainod"),
                path("packages/zainod"),
                path("packages/zainod/CHANGELOG.md"),
                true,
            ),
        ];
        ReleaseConfig::for_test(options, targets)
    }

    fn slug(raw: &str) -> Slug {
        Slug::parse(raw).expect("valid slug")
    }

    fn service(changed: Vec<&str>, store: Arc<dyn ChangesetStore>) -> ChangesetCheckService {
        let changed = changed.into_iter().map(PathBuf::from).collect();
        ChangesetCheckService::new(config(), Arc::new(StubVcs::new(changed)), store)
    }

    /// A store seeded with `(slug, toml)` entries.
    fn store_with(entries: &[(&str, &str)]) -> Arc<MapChangesetStore> {
        let store = MapChangesetStore::new();
        for (s, toml) in entries {
            store.write(&slug(s), toml).expect("seed store");
        }
        Arc::new(store)
    }

    const COVERS_A: &str = r#"
[[changes]]
crate = "zaino-state"
kind = "feature"
description = "A change to A."
"#;

    #[test]
    fn source_under_a_with_covering_changeset_is_ok() {
        let store = store_with(&[("pr-1", COVERS_A)]);
        let svc = service(
            vec!["packages/zaino-state/src/lib.rs", ".changesets/pr-1.toml"],
            store,
        );
        let report = svc.check("dev").expect("check runs");
        assert!(report.is_ok(), "expected ok, got {:?}", report.violations);
    }

    #[test]
    fn covers_b_but_touches_a_reports_target_uncovered() {
        let covers_b = r#"
[[changes]]
crate = "zainod"
kind = "fix"
description = "A change to B."
"#;
        let store = store_with(&[("pr-1", covers_b)]);
        let svc = service(
            vec!["packages/zaino-state/src/lib.rs", ".changesets/pr-1.toml"],
            store,
        );
        let report = svc.check("dev").expect("check runs");
        assert_eq!(
            report.violations,
            vec![Violation::TargetUncovered(crate_name("zaino-state"))]
        );
    }

    #[test]
    fn touched_a_with_no_changeset_file_reports_missing() {
        let svc = service(
            vec!["packages/zaino-state/src/lib.rs"],
            Arc::new(MapChangesetStore::new()),
        );
        let report = svc.check("dev").expect("check runs");
        assert_eq!(
            report.violations,
            vec![Violation::NoChangesetForTouchedTargets]
        );
    }

    #[test]
    fn touched_a_with_empty_changeset_is_waived() {
        let empty = "[empty]\nreason = \"comment-only\"\n";
        let store = store_with(&[("pr-1", empty)]);
        let svc = service(
            vec!["packages/zaino-state/src/lib.rs", ".changesets/pr-1.toml"],
            store,
        );
        let report = svc.check("dev").expect("check runs");
        assert!(report.is_ok(), "expected ok, got {:?}", report.violations);
    }

    #[test]
    fn entry_naming_non_target_reports_unknown() {
        let unknown = r#"
[[changes]]
crate = "zaino-testutils"
kind = "internal"
description = "Touched a non-target crate."
"#;
        let store = store_with(&[("pr-1", unknown)]);
        let svc = service(vec![".changesets/pr-1.toml"], store);
        let report = svc.check("dev").expect("check runs");
        assert_eq!(
            report.violations,
            vec![Violation::UnknownTargetInChangeset(
                "zaino-testutils".to_owned()
            )]
        );
    }

    #[test]
    fn unparseable_changeset_reports_parse_violation() {
        // Both [[changes]] and [empty] present -> a parse error.
        let bad = "[[changes]]\ncrate = \"zaino-state\"\nkind = \"fix\"\ndescription = \"x\"\n[empty]\nreason = \"y\"\n";
        let store = store_with(&[("pr-1", bad)]);
        // Only the (broken) changeset changed, so no target is touched — the
        // parse violation is the sole signal.
        let svc = service(vec![".changesets/pr-1.toml"], store);
        let report = svc.check("dev").expect("check runs");
        assert!(matches!(
            report.violations.as_slice(),
            [Violation::ChangesetParse { file, .. }]
                if file == &PathBuf::from(".changesets/pr-1.toml")
        ));
    }

    #[test]
    fn unfilled_template_reports_its_own_violation_and_leaves_target_uncovered() {
        // The PR touches zaino-state source and adds a changeset file, but the
        // file is still the unedited comments-only scaffold. It must flag the
        // unfilled template *and* leave the touched target uncovered (an empty
        // template waives nothing and covers nothing).
        let template = "# Changeset for this PR — not yet filled in.\n";
        let store = store_with(&[("pr-1", template)]);
        let svc = service(
            vec!["packages/zaino-state/src/lib.rs", ".changesets/pr-1.toml"],
            store,
        );
        let report = svc.check("dev").expect("check runs");
        assert_eq!(
            report.violations,
            vec![
                Violation::UnfilledTemplate(PathBuf::from(".changesets/pr-1.toml")),
                Violation::TargetUncovered(crate_name("zaino-state")),
            ]
        );
    }

    #[test]
    fn only_non_target_files_changed_is_ok() {
        let svc = service(
            vec![
                ".github/workflows/ci.yml",
                "docs/release/pipeline.md",
                "README.md",
            ],
            Arc::new(MapChangesetStore::new()),
        );
        let report = svc.check("dev").expect("check runs");
        assert!(report.is_ok(), "expected ok, got {:?}", report.violations);
    }

    #[test]
    fn accumulated_changeset_not_in_diff_does_not_cover() {
        // The store holds an accumulated changeset covering A (left by a
        // previously-merged PR) plus this PR's own changeset covering only B.
        // This PR touches A source, so — because the accumulated changeset is
        // NOT in the diff — A stays uncovered.
        let covers_b = r#"
[[changes]]
crate = "zainod"
kind = "fix"
description = "A change to B in this PR."
"#;
        let store = store_with(&[("accumulated", COVERS_A), ("pr-99", covers_b)]);
        let svc = service(
            vec!["packages/zaino-state/src/lib.rs", ".changesets/pr-99.toml"],
            store,
        );
        let report = svc.check("dev").expect("check runs");
        assert_eq!(
            report.violations,
            vec![Violation::TargetUncovered(crate_name("zaino-state"))]
        );
    }
}
