use std::path::Path;

use relman_core::ports::{ChangelogError, ChangelogStore};

/// A [`ChangelogStore`] over the real filesystem.
///
/// Paths are used as given (repo-relative when relman runs from the repo root).
/// `read` returns `None` for a not-yet-created file; `write` creates any
/// missing parent directories. Deliberately dumb: it moves Markdown text,
/// never parses or splices it.
#[derive(Default)]
pub struct FsChangelogStore;

impl FsChangelogStore {
    /// Construct the adapter.
    pub fn new() -> Self {
        Self
    }

    fn io_err(path: &Path, source: std::io::Error) -> ChangelogError {
        ChangelogError::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

impl ChangelogStore for FsChangelogStore {
    fn read(&self, path: &Path) -> Result<Option<String>, ChangelogError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(Some(contents)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Self::io_err(path, source)),
        }
    }

    fn write(&self, path: &Path, contents: &str) -> Result<(), ChangelogError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| Self::io_err(path, source))?;
        }
        std::fs::write(path, contents).map_err(|source| Self::io_err(path, source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_missing_is_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangelogStore::new();
        let path = dir.path().join("packages/x/CHANGELOG.md");
        assert_eq!(store.read(&path).expect("read"), None);
    }

    #[test]
    fn write_creates_parents_then_reads_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangelogStore::new();
        let path = dir.path().join("packages/x/CHANGELOG.md");

        store.write(&path, "# Changelog\n").expect("write");
        assert_eq!(
            store.read(&path).expect("read"),
            Some("# Changelog\n".to_owned())
        );
    }

    #[test]
    fn write_overwrites_existing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangelogStore::new();
        let path = dir.path().join("CHANGELOG.md");

        store.write(&path, "first").expect("write one");
        store.write(&path, "second").expect("write two");
        assert_eq!(store.read(&path).expect("read"), Some("second".to_owned()));
    }
}
