use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::ports::{ChangelogError, ChangelogStore};

/// An in-memory [`ChangelogStore`] backed by a `HashMap<PathBuf, String>`.
///
/// Makes domain tests deterministic and I/O-free: `read` returns `None` for an
/// absent path (a not-yet-created changelog), `write` inserts. Interior
/// mutability via a `Mutex` keeps the port's `&self` signature (the real fs
/// adapter is likewise shared).
#[derive(Default)]
pub struct MapChangelogStore {
    files: Mutex<HashMap<PathBuf, String>>,
}

impl MapChangelogStore {
    /// An empty store — every path reads as `None`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the store with `(path, contents)` pairs already present.
    pub fn with_files<I, P>(files: I) -> Self
    where
        I: IntoIterator<Item = (P, String)>,
        P: Into<PathBuf>,
    {
        let map = files.into_iter().map(|(p, c)| (p.into(), c)).collect();
        Self {
            files: Mutex::new(map),
        }
    }

    /// The contents last written (or seeded) for `path`, for assertions.
    pub fn get(&self, path: &Path) -> Option<String> {
        self.lock().get(path).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, String>> {
        // A poisoned mutex here means a test thread already panicked, so naming
        // the invariant is the clearest failure.
        self.files.lock().expect("MapChangelogStore mutex poisoned")
    }
}

impl ChangelogStore for MapChangelogStore {
    fn read(&self, path: &Path) -> Result<Option<String>, ChangelogError> {
        Ok(self.lock().get(path).cloned())
    }

    fn write(&self, path: &Path, contents: &str) -> Result<(), ChangelogError> {
        self.lock().insert(path.to_path_buf(), contents.to_owned());
        Ok(())
    }
}
