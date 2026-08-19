use std::path::PathBuf;
use std::sync::Arc;

use relman_core::ports::{About, ChangesetCheck, Changesets, Versions};

/// The driving ports the CLI needs, injected by the binary's composition
/// root. Add a field per port as relman grows (`Bump`, `Changelog`, …).
pub struct Ctx {
    pub about: Arc<dyn About>,
    pub changesets: Arc<dyn Changesets>,
    pub changeset_check: Arc<dyn ChangesetCheck>,
    pub versions: Arc<dyn Versions>,
    /// The resolved `.changesets/` directory, for rendering created paths.
    pub changesets_dir: PathBuf,
}
