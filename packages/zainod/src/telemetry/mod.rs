//! What zainod tells its operator: logs, `/metrics`, sync progress, egress

use std::path::PathBuf;

use tracing::Span;
use zaino_persistence::{DiskView, IndexKind};
use zaino_sync::IndexHandle;

pub(crate) mod disk;
pub(crate) mod egress;
pub(crate) mod logging;
pub(crate) mod metrics;
pub(crate) mod progress;

/// One enabled index: its log component, directory and writer's handle
#[derive(Clone)]
pub(crate) struct OpenedIndex {
    pub(crate) kind: IndexKind,
    pub(crate) span: Span,
    pub(crate) dir: PathBuf,
    pub(crate) handle: IndexHandle<DiskView>,
}
