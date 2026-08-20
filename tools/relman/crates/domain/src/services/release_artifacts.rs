use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use relman_core::ports::{ArtifactError, ChangesetStore, ReleaseArtifacts, Versions, Workspace};
use relman_core::types::{
    BumpTable, ChangeEntry, Changeset, ChangesetError, CrateName, CycleId, CycleStatus,
    PublishPlan, StoredChangeset, Tag, TagPlan,
};

use crate::render;

/// Computes the release artifacts CI applies — the git tag plan, the release-PR
/// body, and the publish order. Implements the [`ReleaseArtifacts`] driving port
/// over the [`Versions`] driving port (for the [`BumpTable`]), the
/// [`ChangesetStore`] driven port (for the changelog block), and the
/// [`Workspace`] driven port (for topological publish order).
///
/// The derived [`BumpTable`] already arrives in config-target order (and filtered
/// to the bumping crates), so this service needs no direct handle on the config —
/// every ordering/filtering decision was made in the [`Versions`] derivation.
///
/// Every method is a pure planner: it reads and returns, and touches no ref,
/// working tree, or registry. The "already published" guard stays out of here —
/// `publish_plan` only computes the *order* of the crates that bump.
pub struct ReleaseArtifactsService {
    versions: Arc<dyn Versions>,
    changesets: Arc<dyn ChangesetStore>,
    workspace: Arc<dyn Workspace>,
}

impl ReleaseArtifactsService {
    pub fn new(
        versions: Arc<dyn Versions>,
        changesets: Arc<dyn ChangesetStore>,
        workspace: Arc<dyn Workspace>,
    ) -> Self {
        Self {
            versions,
            changesets,
            workspace,
        }
    }

    /// Re-read and parse the whole changeset set, grouping each crate's direct
    /// [`ChangeEntry`]s in the same deterministic traversal order the version
    /// derivation uses (sorted slugs, then file order). That alignment is what
    /// lets the renderer treat a crate's trailing bump reasons as transitive.
    fn entries_by_crate(&self) -> Result<BTreeMap<CrateName, Vec<ChangeEntry>>, ArtifactError> {
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
                    return Err(ArtifactError::ChangesetParse {
                        slug: slug.as_str().to_owned(),
                        error: error.to_string(),
                    });
                }
            };
            // A consumed changeset belongs to a past release's artifacts; skip
            // it so this cycle's PR body / changelog digest never re-lists it.
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

    /// Order the bumping crates so each follows the governed dependencies it
    /// shares this cycle. Kahn's algorithm with config order as the tie-break,
    /// so the output is deterministic; a leftover node means a dependency cycle.
    fn topo_order(
        &self,
        table: &BumpTable,
    ) -> Result<Vec<(CrateName, relman_core::types::Version)>, ArtifactError> {
        let internal_deps = self.workspace.internal_deps()?;

        // The bumping set and its config order (the tie-break for determinism).
        let order: Vec<CrateName> = table
            .bumps()
            .iter()
            .map(|b| b.crate_name().clone())
            .collect();
        let bumping: BTreeSet<CrateName> = order.iter().cloned().collect();

        // Each bumping crate's prerequisites: the crates it depends on that also
        // bump this cycle. Edges to unchanged crates are irrelevant to the
        // order (they are already published).
        let prereqs: BTreeMap<CrateName, BTreeSet<CrateName>> = order
            .iter()
            .map(|node| {
                let deps = internal_deps
                    .get(node)
                    .into_iter()
                    .flatten()
                    .filter(|(dep, _)| bumping.contains(dep))
                    .map(|(dep, _)| dep.clone())
                    .collect();
                (node.clone(), deps)
            })
            .collect();

        let mut done: BTreeSet<CrateName> = BTreeSet::new();
        let mut emitted: Vec<CrateName> = Vec::with_capacity(order.len());
        while emitted.len() < order.len() {
            // The earliest-in-config not-yet-emitted crate whose prerequisites
            // are all satisfied.
            let next = order.iter().find(|node| {
                !done.contains(*node)
                    && prereqs
                        .get(*node)
                        .is_some_and(|ps| ps.iter().all(|p| done.contains(p)))
            });
            match next {
                Some(node) => {
                    done.insert(node.clone());
                    emitted.push(node.clone());
                }
                None => return Err(ArtifactError::DependencyCycle),
            }
        }

        // Pair each ordered crate with its next version from the table.
        Ok(emitted
            .into_iter()
            .filter_map(|name| table.get(&name).map(|b| (name, b.next().clone())))
            .collect())
    }
}

/// The section shells of the release-PR body. Kept as constants so the exact
/// markdown shape is visible in one place and stable under test.
const NOTHING_BUMPS: &str = "_No crates bump this cycle — nothing to release._\n";

impl ReleaseArtifacts for ReleaseArtifactsService {
    fn tags(&self, cycle: &CycleId, rc: Option<u32>) -> Result<TagPlan, ArtifactError> {
        // A soak/prerelease cut tags only the release candidate; version tags
        // are a blessing-time concern.
        if let Some(n) = rc {
            return Ok(TagPlan::new(vec![Tag::cycle_rc(cycle, n)]));
        }

        // A blessing tags the cycle, then one provenance tag per bumping crate
        // (config order, from the derived table).
        let table = self.versions.derive()?;
        let mut tags = vec![Tag::cycle(cycle)];
        for bump in table.bumps() {
            tags.push(Tag::crate_version(bump.crate_name(), bump.next()));
        }
        Ok(TagPlan::new(tags))
    }

    fn pr_body(
        &self,
        cycle: &CycleId,
        status: Option<&CycleStatus>,
    ) -> Result<String, ArtifactError> {
        let table = self.versions.derive()?;

        let mut body = String::new();
        body.push_str(&format!("# Release {}\n\n", Tag::cycle(cycle)));

        // Live-dashboard sections, above the version table: the gate
        // high-water marks and the release-candidate list. Rendered whenever a
        // status is supplied, even if no crate bumps — the watermarks are still
        // worth showing.
        if let Some(status) = status {
            body.push_str(&render::render_gate_watermarks(status));
            body.push('\n');
            body.push_str(&render::render_rc_table(cycle, status));
            body.push('\n');
        }

        if table.is_empty() {
            body.push_str(NOTHING_BUMPS);
            return Ok(body);
        }

        // Version table (derived, since last stable). With a live status it
        // carries the per-target provenance-tag column.
        body.push_str(&render::render_version_table(&table, status.is_some()));

        // Soak status: a stub table CI fills in later. Only in the plain
        // (no-status) view — with a status the RC table above is the real thing.
        if status.is_none() {
            body.push_str("\n## Soak status\n\n");
            body.push_str("<!-- soak status: filled by CI -->\n\n");
            body.push_str("| RC commit | tag | soak |\n");
            body.push_str("| --------- | --- | ---- |\n");
        }

        // Aggregated changelog, reusing the changelog renderer over the same
        // changesets that drove the derivation.
        let by_crate = self.entries_by_crate()?;
        let direct_for = |name: &CrateName| -> Vec<&ChangeEntry> {
            by_crate
                .get(name)
                .map(|v| v.iter().collect())
                .unwrap_or_default()
        };
        let crates: Vec<_> = table
            .bumps()
            .iter()
            .map(|bump| (bump, direct_for(bump.crate_name())))
            .collect();

        body.push_str("\n## Changelog\n\n");
        body.push_str(&render::render_changelog_digest(&crates));

        Ok(body)
    }

    fn publish_plan(&self) -> Result<PublishPlan, ArtifactError> {
        let table = self.versions.derive()?;
        Ok(PublishPlan::new(self.topo_order(&table)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use relman_config::ReleaseConfig;
    use relman_core::mocks::{MapChangesetStore, MapWorkspace};
    use relman_core::types::{ReleaseOptions, Slug, Target, Version, WorkspacePath};

    use crate::services::VersionService;

    fn name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    fn req(raw: &str) -> semver::VersionReq {
        semver::VersionReq::parse(raw).expect("valid version req")
    }

    fn cycle(raw: &str) -> CycleId {
        CycleId::parse(raw).expect("valid cycle id")
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

    fn store_with(entries: &[(&str, &str)]) -> Arc<MapChangesetStore> {
        let store = MapChangesetStore::new();
        for (slug, toml) in entries {
            store
                .write(&Slug::parse(slug).expect("valid slug"), toml)
                .expect("seed store");
        }
        Arc::new(store)
    }

    /// Assemble a service from a changeset store and a workspace.
    fn service(
        names: &[&str],
        store: Arc<MapChangesetStore>,
        workspace: MapWorkspace,
    ) -> ReleaseArtifactsService {
        let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
            config(names),
            store.clone(),
            Arc::new(workspace),
        ));
        // tags/pr_body don't touch the topo sort, so an empty workspace handle
        // suffices here; publish_plan tests build their own edge-carrying one.
        ReleaseArtifactsService::new(versions, store, Arc::new(MapWorkspace::default()))
    }

    #[test]
    fn tags_blessing_lists_cycle_then_per_crate_versions_in_config_order() {
        // zaino-state breaks (0.6.0 -> 0.7.0); zainod bumps transitively.
        let store = store_with(&[(
            "pr-1",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"breaking\"\ndescription=\"x\"\n",
        )]);
        let workspace = MapWorkspace::new(
            vec![
                (name("zaino-state"), version("0.6.0")),
                (name("zainod"), version("0.4.3")),
            ],
            vec![(name("zainod"), name("zaino-state"), req("=0.6.0"))],
        );
        let svc = service(&["zaino-state", "zainod"], store, workspace);

        let plan = svc.tags(&cycle("2026-08-15"), None).expect("tags");
        let names: Vec<&str> = plan.tags().iter().map(|t| t.as_str()).collect();
        assert_eq!(
            names,
            ["cycle-2026-08-15", "zaino-state-v0.7.0", "zainod-v0.4.4",]
        );
    }

    #[test]
    fn tags_prerelease_is_exactly_one_rc_tag() {
        let store = store_with(&[(
            "pr-1",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"feature\"\ndescription=\"x\"\n",
        )]);
        let workspace =
            MapWorkspace::new(vec![(name("zaino-state"), version("0.6.0"))], Vec::new());
        let svc = service(&["zaino-state"], store, workspace);

        let plan = svc.tags(&cycle("2026-08-15"), Some(3)).expect("tags");
        let names: Vec<&str> = plan.tags().iter().map(|t| t.as_str()).collect();
        assert_eq!(names, ["cycle-2026-08-15-rc.3"]);
    }

    /// A diamond among governed crates: D→B, D→C, B→A, C→A. All four bump
    /// directly. Publish order must place A first, then B and C, then D.
    #[test]
    fn publish_plan_topo_sorts_a_diamond() {
        let store = store_with(&[(
            "pr-1",
            "\
[[changes]]
crate=\"a\"
kind=\"feature\"
description=\"a\"

[[changes]]
crate=\"b\"
kind=\"feature\"
description=\"b\"

[[changes]]
crate=\"c\"
kind=\"feature\"
description=\"c\"

[[changes]]
crate=\"d\"
kind=\"feature\"
description=\"d\"
",
        )]);
        // Requirements that all cross on any bump, so every edge forces order.
        let edges = vec![
            (name("d"), name("b"), req("=0.1.0")),
            (name("d"), name("c"), req("=0.1.0")),
            (name("b"), name("a"), req("=0.1.0")),
            (name("c"), name("a"), req("=0.1.0")),
        ];
        let workspace = MapWorkspace::new(
            vec![
                (name("a"), version("0.1.0")),
                (name("b"), version("0.1.0")),
                (name("c"), version("0.1.0")),
                (name("d"), version("0.1.0")),
            ],
            edges.clone(),
        );
        // topo_order reads the workspace edges directly, so hand this service a
        // workspace that carries them (the derivation service gets its own copy).
        let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
            config(&["a", "b", "c", "d"]),
            store.clone(),
            Arc::new(MapWorkspace::new(
                vec![
                    (name("a"), version("0.1.0")),
                    (name("b"), version("0.1.0")),
                    (name("c"), version("0.1.0")),
                    (name("d"), version("0.1.0")),
                ],
                edges,
            )),
        ));
        let svc = ReleaseArtifactsService::new(versions, store, Arc::new(workspace));

        let plan = svc.publish_plan().expect("plan");
        let order: Vec<&str> = plan.entries().iter().map(|(n, _)| n.as_str()).collect();

        let pos = |n: &str| order.iter().position(|x| *x == n).expect("present");
        assert_eq!(order.len(), 4);
        assert!(pos("a") < pos("b"), "A before B");
        assert!(pos("a") < pos("c"), "A before C");
        assert!(pos("b") < pos("d"), "B before D");
        assert!(pos("c") < pos("d"), "C before D");
    }

    #[test]
    fn publish_plan_omits_a_non_bumping_crate() {
        // Only zaino-state bumps; zainod is governed but untouched and absent.
        let store = store_with(&[(
            "pr-1",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"feature\"\ndescription=\"x\"\n",
        )]);
        let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
            config(&["zaino-state", "zainod"]),
            store.clone(),
            Arc::new(MapWorkspace::new(
                vec![
                    (name("zaino-state"), version("0.6.0")),
                    (name("zainod"), version("0.4.3")),
                ],
                Vec::new(),
            )),
        ));
        let svc = ReleaseArtifactsService::new(
            versions,
            store,
            Arc::new(MapWorkspace::new(
                vec![
                    (name("zaino-state"), version("0.6.0")),
                    (name("zainod"), version("0.4.3")),
                ],
                Vec::new(),
            )),
        );

        let plan = svc.publish_plan().expect("plan");
        let order: Vec<&str> = plan.entries().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(order, ["zaino-state"]);
    }

    #[test]
    fn pr_body_has_version_table_soak_placeholder_and_changelog() {
        // zaino-state breaks directly; zainod bumps transitively through it.
        let store = store_with(&[(
            "pr-1",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"breaking\"\n\
             description=\"Replace the sync entrypoint.\"\n",
        )]);
        let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
            config(&["zaino-state", "zainod"]),
            store.clone(),
            Arc::new(MapWorkspace::new(
                vec![
                    (name("zaino-state"), version("0.6.0")),
                    (name("zainod"), version("0.4.3")),
                ],
                vec![(name("zainod"), name("zaino-state"), req("=0.6.0"))],
            )),
        ));
        let svc = ReleaseArtifactsService::new(versions, store, Arc::new(MapWorkspace::default()));

        let body = svc.pr_body(&cycle("2026-08-15"), None).expect("pr body");

        assert!(body.contains("# Release cycle-2026-08-15"), "title: {body}");
        // Version table with correct current -> next for both crates.
        assert!(body.contains("| Crate | Current | Next | Bump |"));
        assert!(body.contains("| zaino-state | 0.6.0 | 0.7.0 | minor |"));
        assert!(body.contains("| zainod | 0.4.3 | 0.4.4 | patch |"));
        // No live-dashboard sections without a status.
        assert!(!body.contains("## Gate watermarks"));
        assert!(!body.contains("## Release candidates"));
        // Soak placeholder (plain view keeps the old shape).
        assert!(body.contains("## Soak status"));
        assert!(body.contains("<!-- soak status: filled by CI -->"));
        // Changelog block: the direct bullet and the crate subsection headings.
        assert!(body.contains("## Changelog"));
        assert!(body.contains("### zaino-state 0.7.0"));
        assert!(body.contains("- Replace the sync entrypoint."));
        assert!(body.contains("### zainod 0.4.4"));
    }

    #[test]
    fn pr_body_with_status_renders_watermarks_rc_and_tag_column() {
        // zaino-state breaks directly; zainod bumps transitively through it.
        let store = store_with(&[(
            "pr-1",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"breaking\"\n\
             description=\"Replace the sync entrypoint.\"\n",
        )]);
        let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
            config(&["zaino-state", "zainod"]),
            store.clone(),
            Arc::new(MapWorkspace::new(
                vec![
                    (name("zaino-state"), version("0.6.0")),
                    (name("zainod"), version("0.4.3")),
                ],
                vec![(name("zainod"), name("zaino-state"), req("=0.6.0"))],
            )),
        ));
        let svc = ReleaseArtifactsService::new(versions, store, Arc::new(MapWorkspace::default()));

        let status = CycleStatus::parse_toml(
            "\
released_cycle = \"cycle-0\"

[watermarks]
dev = \"dd73705\"
rc = \"dd73705\"
release_ready = \"dd73705\"
stable = \"5e3caa1\"

[[rc]]
tag = \"cycle-1-rc.1\"
sha = \"dd73705\"
deployment = \"passed\"
",
        )
        .expect("valid status");

        let body = svc.pr_body(&cycle("1"), Some(&status)).expect("pr body");

        // Gate watermarks table, with rc's latest-tag marker and stable's
        // released-cycle marker.
        assert!(body.contains("## Gate watermarks"), "body: {body}");
        assert!(body.contains("| verified | dev | dd73705 | — |"));
        assert!(body.contains("| candidate | rc | dd73705 | cycle-1-rc.1 |"));
        assert!(body.contains("| released | stable | 5e3caa1 | cycle-0 |"));
        // Release-candidate table with the deployment glyph + word.
        assert!(body.contains("## Release candidates — cycle 1"));
        assert!(body.contains("| rc.1 | dd73705 | cycle-1-rc.1 | ✓ passed |"));
        // Version table gains the per-target Tag column.
        assert!(body.contains("| Crate | Current | Next | Bump | Tag |"));
        assert!(body.contains("| zaino-state | 0.6.0 | 0.7.0 | minor | zaino-state-v0.7.0 |"));
        assert!(body.contains("| zainod | 0.4.3 | 0.4.4 | patch | zainod-v0.4.4 |"));
        // The soak stub is replaced by the real RC table.
        assert!(!body.contains("## Soak status"));
        // Changelog still present.
        assert!(body.contains("## Changelog"));
        assert!(body.contains("### zaino-state 0.7.0"));
    }

    #[test]
    fn pr_body_says_so_when_nothing_bumps() {
        let store: Arc<MapChangesetStore> = Arc::new(MapChangesetStore::new());
        let workspace =
            MapWorkspace::new(vec![(name("zaino-state"), version("0.6.0"))], Vec::new());
        let svc = service(&["zaino-state"], store, workspace);

        let body = svc.pr_body(&cycle("2026-08-15"), None).expect("pr body");
        assert!(body.contains("# Release cycle-2026-08-15"));
        assert!(body.contains("nothing to release"));
        assert!(!body.contains("| Crate |"), "no version table when empty");
    }
}
