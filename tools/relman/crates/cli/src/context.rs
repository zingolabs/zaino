use std::path::PathBuf;
use std::sync::Arc;

use relman_core::ports::{
    About, ApplyBump, Changelog, ChangesetCheck, Changesets, ReleaseArtifacts, Versions,
};

/// The driving ports the CLI needs, injected by the binary's composition
/// root. Add a field per port as relman grows.
pub struct Ctx {
    pub about: Arc<dyn About>,
    pub changesets: Arc<dyn Changesets>,
    pub changeset_check: Arc<dyn ChangesetCheck>,
    pub versions: Arc<dyn Versions>,
    pub apply_bump: Arc<dyn ApplyBump>,
    pub changelog: Arc<dyn Changelog>,
    pub release_artifacts: Arc<dyn ReleaseArtifacts>,
    /// The resolved `.changesets/` directory, for rendering created paths.
    pub changesets_dir: PathBuf,
    /// The resolved root manifest, for naming where pins were updated.
    pub root_manifest: PathBuf,
}
