use std::collections::HashMap;
use std::sync::Mutex;

use crate::ports::{ChangesetStore, ChangesetStoreError};
use crate::types::Slug;

/// An in-memory [`ChangesetStore`] backed by a `HashMap<Slug, String>`.
///
/// Makes domain tests deterministic and I/O-free: `write` inserts, `read` and
/// `exists` and `list` consult the map. Interior mutability via a `Mutex` keeps
/// the port's `&self` signature (the real fs adapter is likewise shared).
#[derive(Default)]
pub struct MapChangesetStore {
    files: Mutex<HashMap<Slug, String>>,
}

impl MapChangesetStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the store with a slug already present, to provoke a collision.
    pub fn with_existing(slug: Slug, contents: &str) -> Self {
        let mut files = HashMap::new();
        files.insert(slug, contents.to_owned());
        Self {
            files: Mutex::new(files),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Slug, String>> {
        self.files.expect_lock()
    }
}

/// Small extension so the mock never `.unwrap()`s a poisoned lock inline; a
/// poisoned mutex here means a test thread already panicked, so naming the
/// invariant is the clearest failure.
trait ExpectLock<T> {
    fn expect_lock(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> ExpectLock<T> for Mutex<T> {
    fn expect_lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().expect("MapChangesetStore mutex poisoned")
    }
}

impl ChangesetStore for MapChangesetStore {
    fn list(&self) -> Result<Vec<Slug>, ChangesetStoreError> {
        Ok(self.lock().keys().cloned().collect())
    }

    fn exists(&self, slug: &Slug) -> Result<bool, ChangesetStoreError> {
        Ok(self.lock().contains_key(slug))
    }

    fn read(&self, slug: &Slug) -> Result<String, ChangesetStoreError> {
        self.lock()
            .get(slug)
            .cloned()
            .ok_or_else(|| ChangesetStoreError::Io {
                slug: slug.as_str().to_owned(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such changeset"),
            })
    }

    fn write(&self, slug: &Slug, contents: &str) -> Result<(), ChangesetStoreError> {
        self.lock().insert(slug.clone(), contents.to_owned());
        Ok(())
    }
}
