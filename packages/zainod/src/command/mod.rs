//! One-shot subcommands: `verify`, `hold` / `resume`, `overload`
//!
//! - hold + overload = marker files in `[serve] state_dir` (CLI writes, running zainod polls each
//!   second: both halves of each contract live in its file)

pub(super) mod hold;
pub(super) mod overload;
pub(super) mod verify;

use std::io;
use std::path::{Path, PathBuf};

use crate::error::IndexerError;

#[derive(Debug, thiserror::Error)]
pub(super) enum MarkerError {
    #[error(transparent)]
    Config(#[from] IndexerError),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{0}")]
    Invalid(String),
}

/// `dir/name` = `contents`, file + directory entry synced (survives a reboot right after)
fn write_marker(dir: &Path, name: &str, contents: &str) -> Result<(), MarkerError> {
    let path = dir.join(name);
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&path, contents))
        .and_then(|()| std::fs::File::open(&path)?.sync_all())
        .and_then(|()| std::fs::File::open(dir)?.sync_all());
    written.map_err(|source| MarkerError::Io { path, source })
}

/// `dir/name` removed, directory synced (`false` = was absent)
fn remove_marker(dir: &Path, name: &str) -> Result<bool, MarkerError> {
    let path = dir.join(name);
    let removed = match std::fs::remove_file(&path) {
        Ok(()) => std::fs::File::open(dir).and_then(|dir| dir.sync_all()).map(|()| true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    };
    removed.map_err(|source| MarkerError::Io { path, source })
}
