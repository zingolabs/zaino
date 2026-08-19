use std::collections::BTreeMap;
use std::sync::Arc;

use relman_config::ReleaseConfig;
use relman_core::ports::{ChangesetStore, DeriveError, Versions, Workspace};
use relman_core::types::{Bump, BumpTable, ChangeKind, Changeset, CrateBump, CrateName, Version};

/// Derives the per-crate version [`BumpTable`] from the accumulated changesets
/// and the workspace crate graph. Implements the [`Versions`] driving port over
/// the [`ChangesetStore`] and [`Workspace`] driven ports and the loaded
/// [`ReleaseConfig`].
///
/// The derivation is deterministic and read-only over the *whole* `.changesets/`
/// set (it never consumes or clears):
///
/// 1. **Direct bumps** — group every `[[changes]]` entry by crate, take the
///    highest [`ChangeKind`], and map it to a [`Bump`] against the crate's
///    current version (applying the pre-1.0 relaxation).
/// 2. **Transitive bumps** — to a fixpoint, any governed dependent whose
///    declared requirement no longer matches a bumped dependency's *new*
///    version needs its `Cargo.toml` updated, forcing at least a `Patch` bump.
///    [`semver::VersionReq::matches`] handles 0.x and 1.x boundaries uniformly.
pub struct VersionService {
    config: ReleaseConfig,
    store: Arc<dyn ChangesetStore>,
    workspace: Arc<dyn Workspace>,
}

/// The direct-bump inputs collected from the changeset set, per crate: the
/// highest kind seen, and the descriptions (in encounter order) that become
/// changelog reasons.
#[derive(Default)]
struct DirectInputs {
    highest_kind: BTreeMap<CrateName, ChangeKind>,
    reasons: BTreeMap<CrateName, Vec<String>>,
}

impl VersionService {
    pub fn new(
        config: ReleaseConfig,
        store: Arc<dyn ChangesetStore>,
        workspace: Arc<dyn Workspace>,
    ) -> Self {
        Self {
            config,
            store,
            workspace,
        }
    }

    /// Whether `name` is a governed target declared in `relman.toml`.
    fn is_target(&self, name: &CrateName) -> bool {
        self.config.target_by_name(name).is_some()
    }

    /// Read and parse the whole changeset set, folding each `WithChanges` entry
    /// into the per-crate highest kind + reasons. `Empty` changesets contribute
    /// nothing; an entry naming a non-target crate is a hard error.
    fn collect_direct(&self) -> Result<DirectInputs, DeriveError> {
        let mut slugs = self.store.list()?;
        // Deterministic traversal order regardless of the store's backing.
        slugs.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let mut inputs = DirectInputs::default();
        for slug in &slugs {
            let raw = self.store.read(slug)?;
            let changeset =
                Changeset::parse_toml(&raw).map_err(|error| DeriveError::ChangesetParse {
                    slug: slug.as_str().to_owned(),
                    error: error.to_string(),
                })?;
            let Changeset::WithChanges(entries) = changeset else {
                continue;
            };
            for entry in entries {
                let name = entry.crate_name();
                if !self.is_target(name) {
                    return Err(DeriveError::UnknownTarget {
                        crate_name: name.as_str().to_owned(),
                    });
                }
                inputs
                    .highest_kind
                    .entry(name.clone())
                    .and_modify(|k| *k = (*k).max(entry.kind()))
                    .or_insert(entry.kind());
                inputs
                    .reasons
                    .entry(name.clone())
                    .or_default()
                    .push(entry.description().as_str().to_owned());
            }
        }
        Ok(inputs)
    }

    /// The current version of `name`, or a typed error if the workspace has none.
    fn current<'v>(
        &self,
        name: &CrateName,
        versions: &'v BTreeMap<CrateName, Version>,
    ) -> Result<&'v Version, DeriveError> {
        versions
            .get(name)
            .ok_or_else(|| DeriveError::MissingVersion {
                crate_name: name.as_str().to_owned(),
            })
    }

    /// The next version `name` would take under `chosen`, if it bumps at all.
    fn next_version(
        &self,
        name: &CrateName,
        chosen: &BTreeMap<CrateName, Bump>,
        versions: &BTreeMap<CrateName, Version>,
    ) -> Result<Option<Version>, DeriveError> {
        match chosen.get(name) {
            Some(bump) => Ok(Some(bump.apply(self.current(name, versions)?))),
            None => Ok(None),
        }
    }

    /// Grow `chosen` to a fixpoint: every governed dependent whose requirement
    /// fails to match a bumped dependency's new version gains at least a
    /// `Patch`. Since `Patch` is the minimum bump, this only ever *adds* crates
    /// — it never lowers an existing (possibly larger) bump.
    fn apply_transitive(
        &self,
        chosen: &mut BTreeMap<CrateName, Bump>,
        internal_deps: &BTreeMap<CrateName, Vec<(CrateName, semver::VersionReq)>>,
        versions: &BTreeMap<CrateName, Version>,
    ) -> Result<(), DeriveError> {
        loop {
            let mut changed = false;
            for (dependent, edges) in internal_deps {
                if chosen.contains_key(dependent) {
                    continue; // Already bumping; a crossing cannot lower it.
                }
                for (dependency, req) in edges {
                    let Some(dep_next) = self.next_version(dependency, chosen, versions)? else {
                        continue; // Dependency does not bump.
                    };
                    if !req.matches(dep_next.as_semver()) {
                        chosen.insert(dependent.clone(), Bump::Patch);
                        changed = true;
                        break;
                    }
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    /// Explain each transitive crossing: for every bumping dependent, the
    /// bumped dependencies whose new version escaped its declared requirement.
    /// Runs after the fixpoint so every referenced `next` version is final.
    fn transitive_reasons(
        &self,
        chosen: &BTreeMap<CrateName, Bump>,
        internal_deps: &BTreeMap<CrateName, Vec<(CrateName, semver::VersionReq)>>,
        versions: &BTreeMap<CrateName, Version>,
    ) -> Result<BTreeMap<CrateName, Vec<String>>, DeriveError> {
        let mut reasons: BTreeMap<CrateName, Vec<String>> = BTreeMap::new();
        for (dependent, edges) in internal_deps {
            if !chosen.contains_key(dependent) {
                continue;
            }
            for (dependency, req) in edges {
                let Some(dep_next) = self.next_version(dependency, chosen, versions)? else {
                    continue;
                };
                if !req.matches(dep_next.as_semver()) {
                    let dep_current = self.current(dependency, versions)?;
                    reasons.entry(dependent.clone()).or_default().push(format!(
                        "dependency `{dependency}` {dep_current}→{dep_next} crossed the requirement `{req}`"
                    ));
                }
            }
        }
        Ok(reasons)
    }
}

impl Versions for VersionService {
    fn derive(&self) -> Result<BumpTable, DeriveError> {
        let versions = self.workspace.versions()?;
        let internal_deps = self.workspace.internal_deps()?;
        let direct = self.collect_direct()?;

        // Direct bumps: highest kind → bump against the crate's current version.
        let mut chosen: BTreeMap<CrateName, Bump> = BTreeMap::new();
        for (name, kind) in &direct.highest_kind {
            let current = self.current(name, &versions)?;
            chosen.insert(name.clone(), Bump::from_kind(*kind, current));
        }

        self.apply_transitive(&mut chosen, &internal_deps, &versions)?;
        let transitive = self.transitive_reasons(&chosen, &internal_deps, &versions)?;

        // Assemble in config-target order, only crates that bump. Reasons are
        // direct descriptions first, then transitive explanations.
        let mut bumps = Vec::new();
        for target in self.config.targets() {
            let name = target.name();
            let Some(bump) = chosen.get(name) else {
                continue;
            };
            let current = self.current(name, &versions)?.clone();
            let next = bump.apply(&current);
            let mut reasons = direct.reasons.get(name).cloned().unwrap_or_default();
            if let Some(extra) = transitive.get(name) {
                reasons.extend(extra.iter().cloned());
            }
            bumps.push(CrateBump::new(name.clone(), current, next, *bump, reasons));
        }
        Ok(BumpTable::new(bumps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use relman_core::mocks::{MapChangesetStore, MapWorkspace};
    use relman_core::ports::ChangesetStore;
    use relman_core::types::{ReleaseOptions, Slug, Target, WorkspacePath};

    fn name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    fn req(raw: &str) -> semver::VersionReq {
        semver::VersionReq::parse(raw).expect("valid version req")
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

    /// A config governing exactly `names`, in the given order.
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

    fn service(
        names: &[&str],
        store: Arc<dyn ChangesetStore>,
        workspace: MapWorkspace,
    ) -> VersionService {
        VersionService::new(config(names), store, Arc::new(workspace))
    }

    /// A workspace with the given `(crate, current-version)` pairs and no edges.
    fn workspace(versions: &[(&str, &str)]) -> MapWorkspace {
        MapWorkspace::new(
            versions
                .iter()
                .map(|(n, v)| (name(n), version(v)))
                .collect(),
            Vec::new(),
        )
    }

    #[test]
    fn pre_1_0_breaking_bumps_minor() {
        let store = store_with(&[(
            "a",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"breaking\"\ndescription=\"x\"\n",
        )]);
        let svc = service(
            &["zaino-state"],
            store,
            workspace(&[("zaino-state", "0.3.1")]),
        );
        let table = svc.derive().expect("derives");

        let bump = table.get(&name("zaino-state")).expect("bumps");
        assert_eq!(bump.current(), &version("0.3.1"));
        assert_eq!(bump.next(), &version("0.4.0"));
        assert_eq!(bump.bump(), Bump::Minor);
        assert_eq!(bump.reasons(), ["x"]);
    }

    #[test]
    fn pre_1_0_feature_and_fix_and_internal_bump_patch() {
        for kind in ["feature", "fix", "internal"] {
            let toml =
                format!("[[changes]]\ncrate=\"zaino-state\"\nkind=\"{kind}\"\ndescription=\"x\"\n");
            let store = store_with(&[("a", &toml)]);
            let svc = service(
                &["zaino-state"],
                store,
                workspace(&[("zaino-state", "0.3.1")]),
            );
            let table = svc.derive().expect("derives");
            let bump = table.get(&name("zaino-state")).expect("bumps");
            assert_eq!(bump.next(), &version("0.3.2"), "kind {kind}");
            assert_eq!(bump.bump(), Bump::Patch, "kind {kind}");
        }
    }

    #[test]
    fn post_1_0_breaking_and_feature() {
        let breaking = store_with(&[(
            "a",
            "[[changes]]\ncrate=\"zainod\"\nkind=\"breaking\"\ndescription=\"x\"\n",
        )]);
        let svc = service(&["zainod"], breaking, workspace(&[("zainod", "1.2.0")]));
        assert_eq!(
            svc.derive()
                .expect("derives")
                .get(&name("zainod"))
                .expect("bumps")
                .next(),
            &version("2.0.0")
        );

        let feature = store_with(&[(
            "a",
            "[[changes]]\ncrate=\"zainod\"\nkind=\"feature\"\ndescription=\"x\"\n",
        )]);
        let svc = service(&["zainod"], feature, workspace(&[("zainod", "1.2.0")]));
        assert_eq!(
            svc.derive()
                .expect("derives")
                .get(&name("zainod"))
                .expect("bumps")
                .next(),
            &version("1.3.0")
        );
    }

    #[test]
    fn highest_kind_wins_across_entries() {
        // A fix and a breaking on the same crate resolve to the breaking bump.
        let toml = "\
[[changes]]
crate=\"zaino-state\"
kind=\"fix\"
description=\"a fix\"

[[changes]]
crate=\"zaino-state\"
kind=\"breaking\"
description=\"a break\"
";
        let store = store_with(&[("a", toml)]);
        let svc = service(
            &["zaino-state"],
            store,
            workspace(&[("zaino-state", "0.3.1")]),
        );
        let table = svc.derive().expect("derives");
        let bump = table.get(&name("zaino-state")).expect("bumps");
        assert_eq!(bump.bump(), Bump::Minor);
        assert_eq!(bump.next(), &version("0.4.0"));
        // Both descriptions are kept as reasons.
        assert_eq!(bump.reasons(), ["a fix", "a break"]);
    }

    #[test]
    fn empty_changeset_contributes_nothing() {
        let store = store_with(&[("a", "[empty]\nreason=\"comment-only\"\n")]);
        let svc = service(
            &["zaino-state"],
            store,
            workspace(&[("zaino-state", "0.3.1")]),
        );
        let table = svc.derive().expect("derives");
        assert!(table.is_empty(), "empty changeset should not bump anything");
    }

    #[test]
    fn no_changesets_bumps_nothing() {
        let store: Arc<dyn ChangesetStore> = Arc::new(MapChangesetStore::new());
        let svc = service(
            &["zaino-state"],
            store,
            workspace(&[("zaino-state", "0.3.1")]),
        );
        assert!(svc.derive().expect("derives").is_empty());
    }

    #[test]
    fn transitive_patch_flows_to_dependents_but_stops_at_a_matching_req() {
        // A(0.3.1) breaking -> 0.4.0. B --^0.3--> A crosses (0.4.0 !~ ^0.3) -> B patches.
        // C --^0.3--> B (second order): B 0.5.0 -> 0.5.1 patch. C's req ^0.5 still
        // matches 0.5.1, so C does NOT bump from B... instead give C a req on B
        // that DOES cross to exercise the second-order flow, and a fourth crate D
        // whose req still matches A and does NOT bump.
        let store = store_with(&[(
            "a",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"breaking\"\ndescription=\"break A\"\n",
        )]);
        let edges = vec![
            // B depends on A with ^0.3 (breaks on 0.4.0).
            (name("b"), name("zaino-state"), req("^0.3")),
            // C depends on B with ^0.5 (breaks on 0.6.0). B is 0.5.0 -> ... wait
            // B only patches, so use a req that crosses B's patch. Instead make C
            // depend on A too, second order via B below.
            (name("c"), name("b"), req("=0.5.0")),
            // D depends on A but tolerates the whole 0.x line up to <0.5.
            (name("d"), name("zaino-state"), req(">=0.3, <0.5")),
        ];
        let workspace = MapWorkspace::new(
            vec![
                (name("zaino-state"), version("0.3.1")),
                (name("b"), version("0.5.0")),
                (name("c"), version("0.2.0")),
                (name("d"), version("0.1.0")),
            ],
            edges,
        );
        let svc = service(&["zaino-state", "b", "c", "d"], store, workspace);
        let table = svc.derive().expect("derives");

        // A: direct breaking -> minor.
        let a = table.get(&name("zaino-state")).expect("A bumps");
        assert_eq!(a.next(), &version("0.4.0"));

        // B: transitive patch (0.4.0 escaped ^0.3).
        let b = table.get(&name("b")).expect("B bumps");
        assert_eq!(b.bump(), Bump::Patch);
        assert_eq!(b.next(), &version("0.5.1"));
        assert_eq!(b.reasons().len(), 1);
        assert!(b.reasons()[0].contains("`zaino-state` 0.3.1→0.4.0"));
        assert!(b.reasons()[0].contains("`^0.3`"));

        // C: second-order transitive patch (B 0.5.0 -> 0.5.1 escaped C's =0.5.0).
        let c = table.get(&name("c")).expect("C bumps");
        assert_eq!(c.bump(), Bump::Patch);
        assert_eq!(c.next(), &version("0.2.1"));
        assert!(c.reasons()[0].contains("`b` 0.5.0→0.5.1"));

        // D: req still matches 0.4.0, so no bump.
        assert!(table.get(&name("d")).is_none(), "D should not bump");
    }

    #[test]
    fn transitive_does_not_downgrade_a_larger_direct_bump() {
        // B directly breaks (pre-1.0 minor) AND depends on A which also bumps and
        // crosses B's req. B must stay minor, not drop to patch.
        let store = store_with(&[
            (
                "a",
                "[[changes]]\ncrate=\"zaino-state\"\nkind=\"breaking\"\ndescription=\"break A\"\n",
            ),
            (
                "b",
                "[[changes]]\ncrate=\"b\"\nkind=\"breaking\"\ndescription=\"break B\"\n",
            ),
        ]);
        let workspace = MapWorkspace::new(
            vec![
                (name("zaino-state"), version("0.3.1")),
                (name("b"), version("0.2.0")),
            ],
            vec![(name("b"), name("zaino-state"), req("^0.3"))],
        );
        let svc = service(&["zaino-state", "b"], store, workspace);
        let table = svc.derive().expect("derives");

        let b = table.get(&name("b")).expect("B bumps");
        assert_eq!(
            b.bump(),
            Bump::Minor,
            "direct minor must win over transitive patch"
        );
        assert_eq!(b.next(), &version("0.3.0"));
        // Direct description first, then the transitive crossing.
        assert_eq!(b.reasons()[0], "break B");
        assert!(b.reasons()[1].contains("`zaino-state` 0.3.1→0.4.0"));
    }

    #[test]
    fn entry_naming_a_non_target_crate_is_an_error() {
        let store = store_with(&[(
            "a",
            "[[changes]]\ncrate=\"zaino-testutils\"\nkind=\"fix\"\ndescription=\"x\"\n",
        )]);
        let svc = service(
            &["zaino-state"],
            store,
            workspace(&[("zaino-state", "0.3.1")]),
        );
        let err = svc.derive().expect_err("non-target entry should error");
        assert!(matches!(
            err,
            DeriveError::UnknownTarget { crate_name } if crate_name == "zaino-testutils"
        ));
    }

    #[test]
    fn missing_workspace_version_is_an_error() {
        let store = store_with(&[(
            "a",
            "[[changes]]\ncrate=\"zaino-state\"\nkind=\"fix\"\ndescription=\"x\"\n",
        )]);
        // Workspace knows no version for the bumping crate.
        let svc = service(&["zaino-state"], store, workspace(&[]));
        let err = svc.derive().expect_err("missing version should error");
        assert!(matches!(
            err,
            DeriveError::MissingVersion { crate_name } if crate_name == "zaino-state"
        ));
    }
}
