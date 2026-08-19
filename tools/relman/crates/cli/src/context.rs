use std::path::PathBuf;
use std::sync::Arc;

use relman_core::ports::{About, Changesets};

/// The driving ports the CLI needs, injected by the binary's composition
/// root. Add a field per port as relman grows (`Versions`, `Bump`, …).
pub struct Ctx {
    pub about: Arc<dyn About>,
    pub changesets: Arc<dyn Changesets>,
    /// The resolved `.changesets/` directory, for rendering created paths.
    pub changesets_dir: PathBuf,
}
