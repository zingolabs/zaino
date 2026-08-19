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

use anyhow::{Context, Result};
use clap::Parser;

use relman_adapters::{
    CargoMetadataWorkspace, FsChangesetStore, GitVcs, RandomSlugSource, TomlEditManifestEditor,
};
use relman_cli::{Cli, Command, Ctx, commands};
use relman_core::ports::{
    ApplyBump, ChangesetCheck, ChangesetStore, Changesets, Clock, ManifestEditor, SlugSource, Vcs,
    Versions, Workspace,
};
use relman_core::types::{CrateName, DateTime, Utc};
use relman_domain::services::{
    AboutService, BumpService, ChangesetCheckService, ChangesetService, VersionService,
};

/// The repo-committed manifest, looked up in the current working directory.
const MANIFEST_NAME: &str = "relman.toml";

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
    }
}

/// Build the driving-port context and hand it to a command.
///
/// Loads `relman.toml` from the current directory, resolves the changesets
/// directory relative to it, and wires the real adapters into the services.
fn with_ctx(f: impl FnOnce(&Ctx) -> Result<()>) -> Result<()> {
    let manifest_path = PathBuf::from(MANIFEST_NAME);
    let config = relman_config::load(&manifest_path)
        .with_context(|| format!("failed to load {MANIFEST_NAME} from the current directory"))?;

    // The manifest lives at the repo root; resolve the changesets dir against
    // that root (the manifest's parent, i.e. the current directory here).
    let changesets_dir = config.options().changesets_dir().as_path().to_path_buf();

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let about = Arc::new(AboutService::new(clock));

    let store: Arc<dyn ChangesetStore> = Arc::new(FsChangesetStore::new(changesets_dir.clone()));
    let slugs: Arc<dyn SlugSource> = Arc::new(RandomSlugSource::new());
    let changesets: Arc<dyn Changesets> = Arc::new(ChangesetService::new(store.clone(), slugs));

    // The manifest lives at the repo root, i.e. the current directory, so git
    // runs there and reports repo-relative paths.
    let vcs: Arc<dyn Vcs> = Arc::new(GitVcs::new(PathBuf::from(".")));

    // The workspace adapter reads resolved versions and dependency edges from
    // the repo-root manifest via `cargo metadata`, filtered to the governed set.
    let governed: BTreeSet<CrateName> = config
        .targets()
        .iter()
        .map(|target| target.name().clone())
        .collect();
    let root_manifest = config.options().root_manifest().as_path().to_path_buf();
    let workspace: Arc<dyn Workspace> =
        Arc::new(CargoMetadataWorkspace::new(root_manifest.clone(), governed));
    let versions: Arc<dyn Versions> = Arc::new(VersionService::new(
        config.clone(),
        store.clone(),
        workspace,
    ));

    // Applies the derived table to the manifests via format-preserving edits.
    let editor: Arc<dyn ManifestEditor> = Arc::new(TomlEditManifestEditor::new());
    let apply_bump: Arc<dyn ApplyBump> = Arc::new(BumpService::new(config.clone(), editor));

    let changeset_check: Arc<dyn ChangesetCheck> =
        Arc::new(ChangesetCheckService::new(config, vcs, store));

    let ctx = Ctx {
        about,
        changesets,
        changeset_check,
        versions,
        apply_bump,
        changesets_dir,
        root_manifest,
    };
    f(&ctx)
}
