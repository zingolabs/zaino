use std::path::PathBuf;

use relman_core::ports::{ChangesetStore, ChangesetStoreError};
use relman_core::types::Slug;

/// The changeset-file extension. A store file is `<slug>.toml`; the stem is the
/// slug.
const TOML_EXT: &str = "toml";

/// A [`ChangesetStore`] over a real directory (the resolved `changesets_dir`).
///
/// Maps a [`Slug`] to `<dir>/<slug>.toml` and back. `write` creates the
/// directory if it is missing; `list`/`read`/`exists` consult `*.toml` files.
/// Deliberately dumb: it moves TOML text, never parses it.
pub struct FsChangesetStore {
    dir: PathBuf,
}

impl FsChangesetStore {
    /// Root the store at `dir`. The directory need not exist yet — `write`
    /// creates it on first use.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The on-disk path for a slug's changeset file.
    fn path_for(&self, slug: &Slug) -> PathBuf {
        self.dir.join(slug.file_name())
    }

    fn io_err(slug: &Slug, source: std::io::Error) -> ChangesetStoreError {
        ChangesetStoreError::Io {
            slug: slug.as_str().to_owned(),
            source,
        }
    }
}

impl ChangesetStore for FsChangesetStore {
    fn list(&self) -> Result<Vec<Slug>, ChangesetStoreError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // A missing store directory is an empty store, not an error.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(source) => return Err(ChangesetStoreError::List { source }),
        };

        let mut slugs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ChangesetStoreError::List { source })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(TOML_EXT) {
                continue;
            }
            // A non-slug stem (unexpected in a managed dir) is skipped rather
            // than failing the whole listing.
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(slug) = Slug::parse(stem)
            {
                slugs.push(slug);
            }
        }
        Ok(slugs)
    }

    fn exists(&self, slug: &Slug) -> Result<bool, ChangesetStoreError> {
        Ok(self.path_for(slug).exists())
    }

    fn read(&self, slug: &Slug) -> Result<String, ChangesetStoreError> {
        std::fs::read_to_string(self.path_for(slug)).map_err(|source| Self::io_err(slug, source))
    }

    fn write(&self, slug: &Slug, contents: &str) -> Result<(), ChangesetStoreError> {
        std::fs::create_dir_all(&self.dir).map_err(|source| Self::io_err(slug, source))?;
        std::fs::write(self.path_for(slug), contents).map_err(|source| Self::io_err(slug, source))
    }

    fn rename(&self, from: &Slug, to: &Slug) -> Result<(), ChangesetStoreError> {
        if !self.path_for(from).exists() {
            return Err(ChangesetStoreError::RenameSourceMissing {
                from: from.as_str().to_owned(),
            });
        }
        if self.path_for(to).exists() {
            return Err(ChangesetStoreError::RenameTargetExists {
                from: from.as_str().to_owned(),
                to: to.as_str().to_owned(),
            });
        }
        std::fs::rename(self.path_for(from), self.path_for(to))
            .map_err(|source| Self::io_err(from, source))
    }

    fn remove(&self, slug: &Slug) -> Result<(), ChangesetStoreError> {
        std::fs::remove_file(self.path_for(slug)).map_err(|source| Self::io_err(slug, source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(raw: &str) -> Slug {
        Slug::parse(raw).expect("valid test slug")
    }

    #[test]
    fn round_trips_two_changesets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangesetStore::new(dir.path().to_path_buf());

        let one = slug("wandering-quokka");
        let two = slug("brisk-heron");
        store.write(&one, "one contents").expect("write one");
        store.write(&two, "two contents").expect("write two");

        assert!(store.exists(&one).expect("exists one"));
        assert!(store.exists(&two).expect("exists two"));
        assert!(!store.exists(&slug("absent-slug")).expect("exists absent"));

        assert_eq!(store.read(&one).expect("read one"), "one contents");
        assert_eq!(store.read(&two).expect("read two"), "two contents");

        let mut listed: Vec<String> = store
            .list()
            .expect("list")
            .iter()
            .map(|s| s.as_str().to_owned())
            .collect();
        listed.sort();
        assert_eq!(listed, ["brisk-heron", "wandering-quokka"]);
    }

    #[test]
    fn list_on_missing_dir_is_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangesetStore::new(dir.path().join("does-not-exist"));
        assert!(store.list().expect("list").is_empty());
    }

    #[test]
    fn write_creates_missing_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangesetStore::new(dir.path().join("nested/changesets"));
        let s = slug("fresh-slug");
        store.write(&s, "body").expect("write into missing dir");
        assert_eq!(store.read(&s).expect("read back"), "body");
    }

    #[test]
    fn rename_moves_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangesetStore::new(dir.path().to_path_buf());
        let from = slug("wandering-quokka");
        let to = slug("pr-1501");
        store.write(&from, "body").expect("write source");

        store.rename(&from, &to).expect("rename should succeed");

        assert!(!store.exists(&from).expect("source gone"));
        assert_eq!(store.read(&to).expect("read target"), "body");
    }

    #[test]
    fn rename_onto_existing_target_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangesetStore::new(dir.path().to_path_buf());
        let from = slug("wandering-quokka");
        let to = slug("pr-1501");
        store.write(&from, "source").expect("write source");
        store.write(&to, "occupied").expect("write target");

        let err = store
            .rename(&from, &to)
            .expect_err("rename onto an existing target must fail");
        assert!(matches!(err, ChangesetStoreError::RenameTargetExists { .. }));
        // Neither file was disturbed.
        assert_eq!(store.read(&from).expect("source intact"), "source");
        assert_eq!(store.read(&to).expect("target intact"), "occupied");
    }

    #[test]
    fn rename_missing_source_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangesetStore::new(dir.path().to_path_buf());
        let err = store
            .rename(&slug("absent-slug"), &slug("pr-1501"))
            .expect_err("renaming an absent source must fail");
        assert!(matches!(err, ChangesetStoreError::RenameSourceMissing { .. }));
    }

    #[test]
    fn remove_deletes_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FsChangesetStore::new(dir.path().to_path_buf());
        let s = slug("wandering-quokka");
        store.write(&s, "body").expect("write");

        store.remove(&s).expect("remove should succeed");

        assert!(!store.exists(&s).expect("file gone"));
        // Removing an absent file surfaces the underlying I/O error.
        assert!(store.remove(&s).is_err());
    }
}
