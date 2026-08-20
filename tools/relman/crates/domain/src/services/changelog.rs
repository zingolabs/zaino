use std::collections::BTreeMap;
use std::sync::Arc;

use relman_config::ReleaseConfig;
use relman_core::ports::{
    Changelog, ChangelogEdit, ChangelogGenError, ChangelogStore, ChangesetStore, Clock, Versions,
};
use relman_core::types::{ChangeEntry, Changeset, ChangesetError, CrateName, StoredChangeset};

use crate::render;

/// Generates Keep-a-Changelog entries for each bumping crate and the workspace.
/// Implements the [`Changelog`] driving port over the [`Versions`],
/// [`ChangesetStore`], [`ChangelogStore`], and [`Clock`] driven ports and the
/// loaded [`ReleaseConfig`].
///
/// The flow is deterministic and read-only over the changeset set: derive the
/// [`BumpTable`], re-read the changesets to recover each crate's parsed
/// entries, render one dated section per bumping crate (plus a workspace
/// section), and splice each into the head of its changelog's version history.
///
/// [`BumpTable`]: relman_core::types::BumpTable
pub struct ChangelogService {
    config: ReleaseConfig,
    versions: Arc<dyn Versions>,
    changesets: Arc<dyn ChangesetStore>,
    changelogs: Arc<dyn ChangelogStore>,
    clock: Arc<dyn Clock>,
}

impl ChangelogService {
    pub fn new(
        config: ReleaseConfig,
        versions: Arc<dyn Versions>,
        changesets: Arc<dyn ChangesetStore>,
        changelogs: Arc<dyn ChangelogStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            versions,
            changesets,
            changelogs,
            clock,
        }
    }

    /// Re-read and parse the whole changeset set, grouping each crate's direct
    /// [`ChangeEntry`]s in the same deterministic traversal order the version
    /// derivation uses (sorted slugs, then file order). That alignment is what
    /// lets the renderer treat a crate's trailing bump reasons as transitive.
    fn entries_by_crate(&self) -> Result<BTreeMap<CrateName, Vec<ChangeEntry>>, ChangelogGenError> {
        let mut slugs = self.changesets.list()?;
        slugs.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let mut by_crate: BTreeMap<CrateName, Vec<ChangeEntry>> = BTreeMap::new();
        for slug in &slugs {
            let raw = self.changesets.read(slug)?;
            let stored = match StoredChangeset::parse_toml(&raw) {
                Ok(stored) => stored,
                // Skip an unfilled template exactly as an `Empty` changeset is
                // skipped below — it carries no entries. The warning is surfaced
                // by the version derivation, which reads the same set.
                Err(ChangesetError::Unfilled) => continue,
                Err(error) => {
                    return Err(ChangelogGenError::ChangesetParse {
                        slug: slug.as_str().to_owned(),
                        error: error.to_string(),
                    });
                }
            };
            // A consumed changeset was folded into a past release's changelog;
            // skip it so it never re-appears in a later cycle's entries.
            if stored.consumed_in().is_some() {
                continue;
            }
            let Changeset::WithChanges(entries) = stored.into_body() else {
                continue;
            };
            for entry in entries {
                by_crate
                    .entry(entry.crate_name().clone())
                    .or_default()
                    .push(entry);
            }
        }
        Ok(by_crate)
    }
}

impl Changelog for ChangelogService {
    fn generate(&self) -> Result<Vec<ChangelogEdit>, ChangelogGenError> {
        let table = self.versions.derive()?;
        if table.is_empty() {
            return Ok(Vec::new());
        }

        let by_crate = self.entries_by_crate()?;
        let date = self.clock.now().date_naive();

        // Borrow each crate's entries as a slice of refs, so the renderer takes
        // `&[&ChangeEntry]` without cloning.
        let direct_for = |name: &CrateName| -> Vec<&ChangeEntry> {
            by_crate
                .get(name)
                .map(|v| v.iter().collect())
                .unwrap_or_default()
        };

        let mut edits = Vec::new();

        // One edit per bumping crate.
        for bump in table.bumps() {
            let name = bump.crate_name();
            let Some(target) = self.config.target_by_name(name) else {
                // Unreachable: derivation only emits declared targets.
                continue;
            };
            let direct = direct_for(name);
            let section = render::render_crate_section(bump, &direct, date);
            let path = target.changelog().as_path();
            let existing = self.changelogs.read(path)?;
            let contents = render::insert_section(existing.as_deref(), &section);
            edits.push(ChangelogEdit::new(path.to_path_buf(), contents, section));
        }

        // One edit for the workspace changelog.
        let workspace_crates: Vec<_> = table
            .bumps()
            .iter()
            .map(|bump| (bump, direct_for(bump.crate_name())))
            .collect();
        let section = render::render_workspace_section(&workspace_crates, date);
        let ws_path = self.config.options().workspace_changelog().as_path();
        let existing = self.changelogs.read(ws_path)?;
        let contents = render::insert_section(existing.as_deref(), &section);
        edits.push(ChangelogEdit::new(ws_path.to_path_buf(), contents, section));

        Ok(edits)
    }

    fn apply(&self) -> Result<Vec<ChangelogEdit>, ChangelogGenError> {
        let edits = self.generate()?;
        for edit in &edits {
            self.changelogs.write(edit.path(), edit.contents())?;
        }
        Ok(edits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use relman_core::mocks::{FixedClock, MapChangelogStore, MapChangesetStore, MapWorkspace};
    use relman_core::types::{DateTime, ReleaseOptions, Slug, Target, Utc, Version, WorkspacePath};

    use crate::services::VersionService;

    fn name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    fn path(raw: &str) -> WorkspacePath {
        WorkspacePath::parse(raw).expect("valid path")
    }

    fn target(crate_name: &str) -> Target {
        Target::new(
            name(crate_name),
            path(&format!("packages/{crate_name}")),
            path(&format!("packages/{crate_name}/CHANGELOG.md")),
            true,
        )
    }

    fn config(names: &[&str]) -> ReleaseConfig {
        let options = ReleaseOptions::new(
            path(".changesets"),
            path("Cargo.toml"),
            path("CHANGELOG.md"),
        );
        ReleaseConfig::for_test(options, names.iter().map(|n| target(n)).collect())
    }

    fn clock() -> Arc<dyn Clock> {
        let at: DateTime<Utc> = "2026-08-19T12:00:00Z".parse().expect("valid timestamp");
        Arc::new(FixedClock::new(at))
    }

    fn store_with(entries: &[(&str, &str)]) -> Arc<MapChangesetStore> {
        let store = MapChangesetStore::new();
        for (slug, toml) in entries {
            store
                .write(&Slug::parse(slug).expect("valid slug"), toml)
                .expect("seed store");
        }
        Arc::new(store)
    }

    /// End-to-end plan for a 2-crate scenario: `zaino-state` breaks directly,
    /// `zainod` bumps only transitively through its dependency on it.
    #[test]
    fn plans_edits_for_direct_and_transitive_crates() {
        let changesets = store_with(&[(
            "pr-1",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"breaking\"\n\
             description=\"Replace the sync entrypoint.\"\n\
             migration=\"Call sync_with(Serial).\"\n",
        )]);

        // zainod depends on zaino-state with =0.6.0, so 0.6.0→0.7.0 crosses.
        let workspace = MapWorkspace::new(
            vec![
                (name("zaino-state"), version("0.6.0")),
                (name("zainod"), version("0.4.3")),
            ],
            vec![(
                name("zainod"),
                name("zaino-state"),
                semver::VersionReq::parse("=0.6.0").expect("valid req"),
            )],
        );

        let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
            config(&["zaino-state", "zainod"]),
            changesets.clone(),
            Arc::new(workspace),
        ));

        // Existing per-crate changelog for zaino-state; the rest are absent.
        let existing_state = "\
# Changelog

## [Unreleased]
### Added

## [0.6.0] - 2026-08-04
### Fixed
- Something old.
";
        let changelogs = Arc::new(MapChangelogStore::with_files([(
            "packages/zaino-state/CHANGELOG.md",
            existing_state.to_owned(),
        )]));

        let svc = ChangelogService::new(
            config(&["zaino-state", "zainod"]),
            versions,
            changesets,
            changelogs.clone(),
            clock(),
        );

        let edits = svc.apply().expect("generates and writes");
        // Two crate edits + the workspace edit.
        assert_eq!(edits.len(), 3);

        let by_path = |p: &str| {
            edits
                .iter()
                .find(|e| e.path().to_str() == Some(p))
                .unwrap_or_else(|| panic!("edit for {p}"))
        };

        // zaino-state: new 0.7.0 section between Unreleased and 0.6.0, with the
        // breaking bullet + migration note.
        let state = by_path("packages/zaino-state/CHANGELOG.md");
        assert!(state.inserted().starts_with("## [0.7.0] - 2026-08-19\n"));
        assert!(state.inserted().contains("### Changed"));
        assert!(
            state
                .inserted()
                .contains("_Migration:_ Call sync_with(Serial).")
        );
        let unreleased = state
            .contents()
            .find("## [Unreleased]")
            .expect("unreleased");
        let new = state.contents().find("## [0.7.0]").expect("new");
        let old = state.contents().find("## [0.6.0]").expect("old");
        assert!(unreleased < new && new < old, "0.7.0 lands between");
        assert!(state.contents().contains("- Something old."), "old kept");

        // zainod: created fresh (was absent) with a transitive Changed bullet.
        let daemon = by_path("packages/zainod/CHANGELOG.md");
        assert!(daemon.contents().starts_with("# Changelog\n"));
        assert!(daemon.inserted().starts_with("## [0.4.4] - 2026-08-19\n"));
        assert!(daemon.inserted().contains("### Changed"));
        assert!(daemon.inserted().contains("`zaino-state` 0.6.0→0.7.0"));

        // Workspace: date heading + both crates.
        let ws = by_path("CHANGELOG.md");
        assert!(ws.inserted().starts_with("## [2026-08-19]\n"));
        assert!(ws.inserted().contains("### zaino-state 0.7.0"));
        assert!(ws.inserted().contains("### zainod 0.4.4"));

        // apply() actually wrote through the store.
        assert_eq!(
            changelogs
                .get(std::path::Path::new("packages/zainod/CHANGELOG.md"))
                .as_deref(),
            Some(daemon.contents())
        );
    }

    #[test]
    fn no_changesets_plans_no_edits() {
        let changesets: Arc<MapChangesetStore> = Arc::new(MapChangesetStore::new());
        let workspace =
            MapWorkspace::new(vec![(name("zaino-state"), version("0.6.0"))], Vec::new());
        let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
            config(&["zaino-state"]),
            changesets.clone(),
            Arc::new(workspace),
        ));
        let svc = ChangelogService::new(
            config(&["zaino-state"]),
            versions,
            changesets,
            Arc::new(MapChangelogStore::new()),
            clock(),
        );
        assert!(svc.generate().expect("generates").is_empty());
    }
}
