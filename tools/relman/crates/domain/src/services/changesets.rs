use std::sync::Arc;

use relman_core::ports::{ChangesetStore, Changesets, ChangesetsError, NewChangeset, SlugSource};
use relman_core::types::{Changeset, Slug};

/// How many distinct candidate slugs to try before giving up on uniqueness.
///
/// Collisions are astronomically unlikely with a real random source, so a
/// small budget is ample: exhausting it means the source is degenerate, which
/// the [`ChangesetsError::NoUniqueSlug`] error surfaces.
const MAX_SLUG_TRIES: usize = 10;

/// The commented scaffold written by `relman changeset new` (no `--empty`).
///
/// Raw text meant to be edited: every line is a comment, so the file is inert
/// until the author fills in a real `[[changes]]` block. This is why the store
/// is string-based — relman writes a template, the human writes the changeset.
const TEMPLATE: &str = "\
# Changeset for this PR. One file per PR; describe each change to a governed
# (published) crate, at a semantic level. CI aggregates every changeset since
# the last release into per-crate version bumps and changelog lines.
#
# Uncomment and fill in one [[changes]] block per governed public change.
# Multiple crates (and multiple changes to one crate) each get their own block.
#
# [[changes]]
# crate       = \"zaino-state\"   # a governed crate declared in relman.toml
# kind        = \"feature\"       # breaking | feature | fix | internal
# description = \"One operator-facing changelog line, plain language.\"
#
# Optional fields:
# section   = \"Added\"          # override the default Keep-a-Changelog section
# migration = \"Upgrade notes.\" # expected on `kind = \"breaking\"`
# issues    = [\"#987\"]         # extra issue refs (the PR is linked automatically)
#
# One [[changes]] entry per governed public change — even for the same crate at
# the same kind. Internal-only changes may be collapsed into one
# `kind = \"internal\"` entry.
#
# If this PR touches governed-crate source but is genuinely release-irrelevant
# (comment- or test-only), replace this file with the no-op form instead:
#   relman changeset new --empty \"<reason>\"
";

/// Authors new changeset files. Implements the [`Changesets`] driving port over
/// the [`ChangesetStore`] and [`SlugSource`] driven ports.
///
/// Its whole job: pick a slug that doesn't already exist, render the requested
/// shape to TOML text, write it, and return the chosen slug.
pub struct ChangesetService {
    store: Arc<dyn ChangesetStore>,
    slugs: Arc<dyn SlugSource>,
}

impl ChangesetService {
    pub fn new(store: Arc<dyn ChangesetStore>, slugs: Arc<dyn SlugSource>) -> Self {
        Self { store, slugs }
    }

    /// Draw candidate slugs until one is free, up to [`MAX_SLUG_TRIES`].
    fn unique_slug(&self) -> Result<Slug, ChangesetsError> {
        for _ in 0..MAX_SLUG_TRIES {
            let candidate = self.slugs.generate();
            if !self.store.exists(&candidate)? {
                return Ok(candidate);
            }
        }
        Err(ChangesetsError::NoUniqueSlug {
            tries: MAX_SLUG_TRIES,
        })
    }
}

impl Changesets for ChangesetService {
    fn new(&self, req: NewChangeset) -> Result<Slug, ChangesetsError> {
        let contents = match req {
            NewChangeset::Empty { reason } => Changeset::empty(reason).to_toml(),
            NewChangeset::Template => TEMPLATE.to_owned(),
        };
        let slug = self.unique_slug()?;
        self.store.write(&slug, &contents)?;
        Ok(slug)
    }

    fn list(&self) -> Result<Vec<Slug>, ChangesetsError> {
        let mut slugs = self.store.list()?;
        slugs.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(slugs)
    }

    fn rename_to_pr(&self, pr: u32) -> Result<Vec<Slug>, ChangesetsError> {
        // Only the author's random-slug files belong to this PR; accumulated
        // `pr-*` files from earlier merged PRs are already canonical and left
        // alone. Sort for deterministic ordinal assignment.
        let mut sources: Vec<Slug> = self
            .store
            .list()?
            .into_iter()
            .filter(|slug| !slug.is_canonical_pr())
            .collect();
        sources.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let mut renamed = Vec::with_capacity(sources.len());
        for (index, from) in sources.iter().enumerate() {
            let to = Slug::for_pr(pr, index);
            self.store.rename(from, &to)?;
            renamed.push(to);
        }
        Ok(renamed)
    }

    fn clear(&self) -> Result<Vec<Slug>, ChangesetsError> {
        let mut removed = self.store.list()?;
        removed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for slug in &removed {
            self.store.remove(slug)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relman_core::mocks::{MapChangesetStore, SequenceSlugSource};
    use relman_core::ports::ChangesetStoreError;
    use relman_core::types::Description;

    fn slug(raw: &str) -> Slug {
        Slug::parse(raw).expect("valid test slug")
    }

    fn service(store: Arc<dyn ChangesetStore>, slugs: Vec<Slug>) -> ChangesetService {
        ChangesetService::new(store, Arc::new(SequenceSlugSource::new(slugs)))
    }

    #[test]
    fn new_empty_writes_parseable_empty_changeset() {
        let store = Arc::new(MapChangesetStore::new());
        let svc = service(store.clone(), vec![slug("wandering-quokka")]);

        let reason = Description::parse("Comment-only fix; no API change.").expect("non-empty");
        let written = svc
            .new(NewChangeset::Empty { reason })
            .expect("empty changeset should be created");

        assert_eq!(written.as_str(), "wandering-quokka");
        let raw = store
            .read(&written)
            .expect("written slug should be readable");
        let parsed = Changeset::parse_toml(&raw).expect("empty changeset should parse");
        let Changeset::Empty { reason } = parsed else {
            panic!("expected Empty");
        };
        assert_eq!(reason.as_str(), "Comment-only fix; no API change.");
    }

    #[test]
    fn new_template_writes_the_scaffold() {
        let store = Arc::new(MapChangesetStore::new());
        let svc = service(store.clone(), vec![slug("brisk-heron")]);

        let written = svc
            .new(NewChangeset::Template)
            .expect("template changeset should be created");

        assert_eq!(written.as_str(), "brisk-heron");
        let raw = store
            .read(&written)
            .expect("written slug should be readable");
        assert_eq!(raw, TEMPLATE);
        // The scaffold is inert (all comments) until a human edits it.
        assert!(raw.contains("[[changes]]"));
        assert!(raw.contains("--empty"));
    }

    #[test]
    fn collision_on_first_slug_falls_through_to_second() {
        // Seed the store so the first candidate collides; the source hands out
        // the taken slug first, then a free one.
        let taken = slug("wandering-quokka");
        let store = Arc::new(MapChangesetStore::with_existing(
            taken.clone(),
            "[empty]\nreason = \"x\"\n",
        ));
        let svc = service(store.clone(), vec![taken, slug("brisk-heron")]);

        let reason = Description::parse("second try").expect("non-empty");
        let written = svc
            .new(NewChangeset::Empty { reason })
            .expect("should fall through to the free slug");

        assert_eq!(written.as_str(), "brisk-heron");
    }

    /// The store's current slugs, as sorted `&str`s, for terse assertions.
    fn slugs_in(store: &MapChangesetStore) -> Vec<String> {
        let mut names: Vec<String> = store
            .list()
            .expect("list")
            .iter()
            .map(|s| s.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    fn as_strs(slugs: &[Slug]) -> Vec<&str> {
        slugs.iter().map(Slug::as_str).collect()
    }

    #[test]
    fn rename_to_pr_renames_only_the_author_file() {
        let store = Arc::new(MapChangesetStore::new());
        store.write(&slug("wandering-quokka"), "[empty]\n").expect("seed author");
        // An accumulated changeset from an earlier merged PR — already canonical.
        store.write(&slug("pr-1490"), "[empty]\n").expect("seed accumulated");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let renamed = svc.rename_to_pr(1501).expect("rename should succeed");

        assert_eq!(as_strs(&renamed), ["pr-1501"]);
        assert_eq!(slugs_in(&store), ["pr-1490", "pr-1501"]);
    }

    #[test]
    fn rename_to_pr_numbers_multiple_author_files_deterministically() {
        let store = Arc::new(MapChangesetStore::new());
        store.write(&slug("wandering-quokka"), "a").expect("seed one");
        store.write(&slug("brisk-heron"), "b").expect("seed two");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let renamed = svc.rename_to_pr(1501).expect("rename should succeed");

        // Sorted sources: brisk-heron < wandering-quokka, so brisk-heron is first.
        assert_eq!(as_strs(&renamed), ["pr-1501", "pr-1501-2"]);
        assert_eq!(slugs_in(&store), ["pr-1501", "pr-1501-2"]);
    }

    #[test]
    fn rename_to_pr_errors_when_target_already_exists() {
        let store = Arc::new(MapChangesetStore::new());
        store.write(&slug("wandering-quokka"), "a").expect("seed author");
        // A stale `pr-1501` already occupies the canonical target.
        store.write(&slug("pr-1501"), "occupied").expect("seed target");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let err = svc
            .rename_to_pr(1501)
            .expect_err("colliding target must error");
        assert!(matches!(
            err,
            ChangesetsError::Store(ChangesetStoreError::RenameTargetExists { .. })
        ));
    }

    #[test]
    fn rename_to_pr_is_a_noop_without_author_files() {
        let store = Arc::new(MapChangesetStore::new());
        store.write(&slug("pr-1490"), "a").expect("seed accumulated");
        store.write(&slug("pr-1491"), "b").expect("seed accumulated");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let renamed = svc.rename_to_pr(1501).expect("no-op should succeed");

        assert!(renamed.is_empty());
        assert_eq!(slugs_in(&store), ["pr-1490", "pr-1491"]);
    }

    #[test]
    fn clear_empties_the_store_and_reports_removed() {
        let store = Arc::new(MapChangesetStore::new());
        store.write(&slug("wandering-quokka"), "a").expect("seed one");
        store.write(&slug("pr-1490"), "b").expect("seed two");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let removed = svc.clear().expect("clear should succeed");

        assert_eq!(as_strs(&removed), ["pr-1490", "wandering-quokka"]);
        assert!(slugs_in(&store).is_empty());
    }

    #[test]
    fn exhausting_the_retry_budget_errors() {
        // The only slug the source ever hands out is already taken, so every
        // retry collides.
        let taken = slug("wandering-quokka");
        let store = Arc::new(MapChangesetStore::with_existing(
            taken.clone(),
            "[empty]\nreason = \"x\"\n",
        ));
        let svc = service(store, vec![taken]);

        let reason = Description::parse("never lands").expect("non-empty");
        let err = svc
            .new(NewChangeset::Empty { reason })
            .expect_err("should exhaust the retry budget");
        assert!(matches!(err, ChangesetsError::NoUniqueSlug { tries } if tries == MAX_SLUG_TRIES));
    }
}
