//! Composition root.
//!
//! The only place that names concrete adapters. It constructs adapters, wires
//! them into services, and dispatches CLI commands. Everything else depends on
//! ports, not implementations — so swapping an adapter (e.g. a fixed clock for
//! the system one) touches only this file.
//!
//! Two live threads run through the hexagon: `relman about` (CLI → `About` →
//! `AboutService` → `Clock`) and `relman changeset new` (CLI → `Changesets` →
//! `ChangesetService` → `ChangesetStore` + `SlugSource`). Later slices add the
//! remaining release adapters and commands.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;

use relman_adapters::{
    CargoMetadataWorkspace, FsChangelogStore, FsChangesetStore, GitVcs, RandomSlugSource,
    TomlEditManifestEditor,
};
use relman_cli::{Cli, Command, Ctx, commands};
use relman_core::ports::{
    ApplyBump, Changelog, ChangelogStore, ChangesetCheck, ChangesetStore, Changesets, Clock,
    ManifestEditor, ReleaseArtifacts, SlugSource, Vcs, Versions, Workspace,
};
use relman_core::types::{CrateName, DateTime, Utc};
use relman_domain::services::{
    AboutService, BumpService, ChangelogService, ChangesetCheckService, ChangesetService,
    ReleaseArtifactsService, VersionService,
};

/// The repo-committed manifest, discovered by walking up from the current
/// directory (like `git`/`cargo` find their roots).
const MANIFEST_NAME: &str = "relman.toml";

/// Find the directory containing `relman.toml`, starting at the current
/// directory and walking up to the filesystem root. This lets `relman` run
/// from any subdirectory of the repo, resolving all paths against the root.
fn find_manifest_dir() -> Result<PathBuf> {
    let start = std::env::current_dir().context("failed to read the current directory")?;
    let mut dir = start.as_path();
    loop {
        if dir.join(MANIFEST_NAME).is_file() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => bail!(
                "could not find {MANIFEST_NAME} in {} or any parent directory",
                start.display()
            ),
        }
    }
}

/// Real system-clock adapter for the [`Clock`] driven port.
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::About(args) => with_ctx(|ctx| {
            commands::about::run(args, ctx);
            Ok(())
        }),
        Command::Changeset(args) => with_ctx(|ctx| {
            commands::changeset::run(args, ctx)?;
            Ok(())
        }),
        Command::Versions(args) => with_ctx(|ctx| {
            commands::versions::run(args, ctx)?;
            Ok(())
        }),
        Command::Bump(args) => with_ctx(|ctx| {
            commands::bump::run(args, ctx)?;
            Ok(())
        }),
        Command::Changelog(args) => with_ctx(|ctx| {
            commands::changelog::run(args, ctx)?;
            Ok(())
        }),
        Command::Tags(args) => with_ctx(|ctx| {
            commands::tags::run(args, ctx)?;
            Ok(())
        }),
        Command::PrBody(args) => with_ctx(|ctx| {
            commands::pr_body::run(args, ctx)?;
            Ok(())
        }),
        Command::PublishPlan(args) => with_ctx(|ctx| {
            commands::publish_plan::run(args, ctx)?;
            Ok(())
        }),
    }
}

/// Build the driving-port context and hand it to a command.
///
/// Loads `relman.toml` from the current directory, resolves the changesets
/// directory relative to it, and wires the real adapters into the services.
fn with_ctx(f: impl FnOnce(&Ctx) -> Result<()>) -> Result<()> {
    let root_dir = find_manifest_dir()?;
    let manifest_path = root_dir.join(MANIFEST_NAME);
    let config = relman_config::load(&manifest_path)
        .with_context(|| format!("failed to load {}", manifest_path.display()))?;

    // Every manifest-declared path is relative to the repo root (where
    // `relman.toml` lives); resolve them against the discovered root so the
    // command works regardless of the current directory.
    let changesets_dir = root_dir.join(config.options().changesets_dir().as_path());

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let about = Arc::new(AboutService::new(clock));

    let store: Arc<dyn ChangesetStore> = Arc::new(FsChangesetStore::new(changesets_dir.clone()));
    let slugs: Arc<dyn SlugSource> = Arc::new(RandomSlugSource::new());
    let changesets: Arc<dyn Changesets> = Arc::new(ChangesetService::new(store.clone(), slugs));

    // Run git in the repo root so it reports paths relative to that root,
    // matching the target `path`s in `relman.toml`.
    let vcs: Arc<dyn Vcs> = Arc::new(GitVcs::new(root_dir.clone()));

    // The workspace adapter reads resolved versions and dependency edges from
    // the repo-root manifest via `cargo metadata`, filtered to the governed set.
    let governed: BTreeSet<CrateName> = config
        .targets()
        .iter()
        .map(|target| target.name().clone())
        .collect();
    let root_manifest = root_dir.join(config.options().root_manifest().as_path());
    let workspace: Arc<dyn Workspace> =
        Arc::new(CargoMetadataWorkspace::new(root_manifest.clone(), governed));
    let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
        config.clone(),
        store.clone(),
        workspace.clone(),
    ));

    // Applies the derived table to the manifests via format-preserving edits.
    let editor: Arc<dyn ManifestEditor> = Arc::new(TomlEditManifestEditor::new());
    let apply_bump: Arc<dyn ApplyBump> = Arc::new(BumpService::new(config.clone(), editor));

    // Generates changelog sections and splices them into the per-crate and
    // workspace `CHANGELOG.md` files (a fresh clock: the earlier one moved into
    // the about service).
    let changelog_store: Arc<dyn ChangelogStore> = Arc::new(FsChangelogStore::new());
    let changelog: Arc<dyn Changelog> = Arc::new(ChangelogService::new(
        config.clone(),
        versions.clone(),
        store.clone(),
        changelog_store,
        Arc::new(SystemClock),
    ));

    // Computes the release artifacts (tag plan, PR body, publish order) as pure
    // plans for CI to apply — reuses the derived table, the changesets, and the
    // crate graph, and mutates nothing.
    let release_artifacts: Arc<dyn ReleaseArtifacts> = Arc::new(ReleaseArtifactsService::new(
        versions.clone(),
        store.clone(),
        workspace,
    ));

    let changeset_check: Arc<dyn ChangesetCheck> =
        Arc::new(ChangesetCheckService::new(config, vcs, store));

    let ctx = Ctx {
        about,
        changesets,
        changeset_check,
        versions,
        apply_bump,
        changelog,
        release_artifacts,
        changesets_dir,
        root_manifest,
    };
    f(&ctx)
}
