use std::sync::Arc;

use relman_core::ports::{
    ChangesetStore, Changesets, ChangesetsError, ConsumedLedgerStore, NewChangeset, SlugSource,
    UidSource,
};
use relman_core::types::{CONSUMED_IN_KEY, Changeset, CycleId, Slug, StoredChangeset, Uid};

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
/// the [`ChangesetStore`], [`SlugSource`], and [`UidSource`] driven ports.
///
/// Its whole job: mint an immutable id, pick a slug that doesn't already exist,
/// render the requested shape (id-stamped) to TOML text, write it, and return
/// the chosen slug.
pub struct ChangesetService {
    store: Arc<dyn ChangesetStore>,
    slugs: Arc<dyn SlugSource>,
    uids: Arc<dyn UidSource>,
    /// The consumed-UID ledger store. [`consume`](Changesets::consume) appends
    /// each newly-consumed changeset's id here so a later derivation on `dev` can
    /// exclude it by id even before the per-file `consumed_in` mark backports.
    ledger: Arc<dyn ConsumedLedgerStore>,
}

impl ChangesetService {
    pub fn new(
        store: Arc<dyn ChangesetStore>,
        slugs: Arc<dyn SlugSource>,
        uids: Arc<dyn UidSource>,
        ledger: Arc<dyn ConsumedLedgerStore>,
    ) -> Self {
        Self {
            store,
            slugs,
            uids,
            ledger,
        }
    }

    /// Map a changeset-parse failure to a [`ChangesetsError::Parse`] carrying
    /// the offending slug — the shared shape for the consume path's reads.
    fn parse_error(slug: &Slug, error: impl std::fmt::Display) -> ChangesetsError {
        ChangesetsError::Parse {
            slug: slug.as_str().to_owned(),
            error: error.to_string(),
        }
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
        // Every changeset gets an immutable id at creation — before it is
        // written — so its identity is stamped from birth regardless of shape.
        let id = self.uids.generate();
        let contents = match req {
            NewChangeset::Empty { reason } => {
                StoredChangeset::new(Some(id), None, Changeset::empty(reason)).to_toml()
            }
            NewChangeset::Template => template_with_id(&id),
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

    fn pending(&self) -> Result<Vec<Slug>, ChangesetsError> {
        // `list()` already returns the slugs sorted.
        let mut pending = Vec::new();
        for slug in self.list()? {
            let raw = self.store.read(&slug)?;
            let marker =
                StoredChangeset::consumed_marker(&raw).map_err(|e| Self::parse_error(&slug, e))?;
            if marker.is_none() {
                pending.push(slug);
            }
        }
        Ok(pending)
    }

    fn consume(&self, cycle: &CycleId) -> Result<Vec<Slug>, ChangesetsError> {
        let pending = self.pending()?;
        // `pending` already excludes anything with a `consumed_in` mark, so a
        // re-consume finds nothing and never rewrites the ledger — idempotent.
        if pending.is_empty() {
            return Ok(pending);
        }

        let mut ledger = self.ledger.read()?;
        for slug in &pending {
            let raw = self.store.read(slug)?;
            // Record the changeset's id in the ledger before stamping. A legacy
            // changeset with no id contributes no entry (nothing to key on) — it
            // is still stamped below. `insert` is idempotent on a duplicate id.
            if let Some(id) =
                StoredChangeset::id_marker(&raw).map_err(|e| Self::parse_error(slug, e))?
            {
                ledger.insert(id, cycle.clone(), Some(slug.as_str().to_owned()));
            }
            let stamped = stamp_consumed(&raw, cycle).map_err(|e| Self::parse_error(slug, e))?;
            self.store.write(slug, &stamped)?;
        }
        self.ledger.write(&ledger)?;
        Ok(pending)
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

/// Render the commented [`TEMPLATE`] scaffold with a real `id` line prepended,
/// so a templated changeset carries its machine-assigned identity from birth.
///
/// The `id` is a bare top-level key emitted *before* the commented body, which
/// keeps it at the document root (a bare key after a `[[changes]]` header would
/// bind to that table). The body itself stays an unfilled template — no
/// `[[changes]]`/`[empty]` — so the dev-gate's unfilled detection still fires
/// until the author edits it; only the identity is real.
fn template_with_id(id: &Uid) -> String {
    format!(
        "# Machine-assigned changeset identity — do not edit.\n\
         id = \"{id}\"\n\
         {TEMPLATE}"
    )
}

/// Stamp `consumed_in = "<cycle>"` into a changeset's raw TOML text, preserving
/// all existing comments and formatting. A format-preserving edit — hence
/// `toml_edit` rather than the value-model round-trip — so a consumed changeset
/// survives on disk as a faithful ledger entry. Overwrites any existing mark
/// (the consume path only ever calls this on pending files, so that is moot in
/// practice, but it keeps the edit total).
fn stamp_consumed(raw: &str, cycle: &CycleId) -> Result<String, toml_edit::TomlError> {
    let mut doc = raw.parse::<toml_edit::DocumentMut>()?;
    doc[CONSUMED_IN_KEY] = toml_edit::value(cycle.as_str());
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use relman_core::mocks::{
        MapChangesetStore, MapConsumedLedgerStore, SequenceSlugSource, SequenceUidSource,
    };
    use relman_core::ports::{ChangesetStoreError, ConsumedLedgerStore};
    use relman_core::types::{ChangesetError, Description};

    /// A canonical UUIDv7 the test uid source hands out, so a created changeset's
    /// id is deterministic.
    const SAMPLE_UID: &str = "018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f";

    fn slug(raw: &str) -> Slug {
        Slug::parse(raw).expect("valid test slug")
    }

    fn uid(raw: &str) -> Uid {
        Uid::parse(raw).expect("valid test uid")
    }

    fn service(store: Arc<dyn ChangesetStore>, slugs: Vec<Slug>) -> ChangesetService {
        service_with_ledger(store, slugs, Arc::new(MapConsumedLedgerStore::new()))
    }

    /// As [`service`], but with a caller-visible ledger store so a test can
    /// inspect what `consume` appended.
    fn service_with_ledger(
        store: Arc<dyn ChangesetStore>,
        slugs: Vec<Slug>,
        ledger: Arc<dyn ConsumedLedgerStore>,
    ) -> ChangesetService {
        ChangesetService::new(
            store,
            Arc::new(SequenceSlugSource::new(slugs)),
            Arc::new(SequenceUidSource::new(vec![uid(SAMPLE_UID)])),
            ledger,
        )
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
        let stored = StoredChangeset::parse_toml(&raw).expect("empty changeset should parse");
        // The changeset carries its machine-assigned id from birth.
        assert_eq!(stored.id(), Some(&uid(SAMPLE_UID)));
        let Changeset::Empty { reason } = stored.body() else {
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
        // The scaffold now carries a real id line above the commented body.
        assert!(raw.contains(&format!("id = \"{SAMPLE_UID}\"")));
        assert!(raw.contains("# Machine-assigned changeset identity — do not edit."));
        assert!(raw.contains(TEMPLATE));
        // The body is still inert (all comments) until a human edits it.
        assert!(raw.contains("[[changes]]"));
        assert!(raw.contains("--empty"));

        // The id is readable, but the body still classifies as unfilled — so the
        // dev-gate's unfilled detection is not regressed by the stamped identity.
        assert_eq!(
            StoredChangeset::consumed_marker(&raw).expect("marker reads"),
            None
        );
        assert!(matches!(
            StoredChangeset::parse_toml(&raw),
            Err(ChangesetError::Unfilled)
        ));
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
        store
            .write(&slug("wandering-quokka"), "[empty]\n")
            .expect("seed author");
        // An accumulated changeset from an earlier merged PR — already canonical.
        store
            .write(&slug("pr-1490"), "[empty]\n")
            .expect("seed accumulated");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let renamed = svc.rename_to_pr(1501).expect("rename should succeed");

        assert_eq!(as_strs(&renamed), ["pr-1501"]);
        assert_eq!(slugs_in(&store), ["pr-1490", "pr-1501"]);
    }

    #[test]
    fn rename_to_pr_numbers_multiple_author_files_deterministically() {
        let store = Arc::new(MapChangesetStore::new());
        store
            .write(&slug("wandering-quokka"), "a")
            .expect("seed one");
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
        store
            .write(&slug("wandering-quokka"), "a")
            .expect("seed author");
        // A stale `pr-1501` already occupies the canonical target.
        store
            .write(&slug("pr-1501"), "occupied")
            .expect("seed target");
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
        store
            .write(&slug("pr-1490"), "a")
            .expect("seed accumulated");
        store
            .write(&slug("pr-1491"), "b")
            .expect("seed accumulated");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let renamed = svc.rename_to_pr(1501).expect("no-op should succeed");

        assert!(renamed.is_empty());
        assert_eq!(slugs_in(&store), ["pr-1490", "pr-1491"]);
    }

    #[test]
    fn clear_empties_the_store_and_reports_removed() {
        let store = Arc::new(MapChangesetStore::new());
        store
            .write(&slug("wandering-quokka"), "a")
            .expect("seed one");
        store.write(&slug("pr-1490"), "b").expect("seed two");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let removed = svc.clear().expect("clear should succeed");

        assert_eq!(as_strs(&removed), ["pr-1490", "wandering-quokka"]);
        assert!(slugs_in(&store).is_empty());
    }

    /// A filled changeset with a leading comment, to prove `consume` preserves
    /// both the body and the surrounding formatting.
    const FILLED_WITH_COMMENT: &str = "\
# This PR replaces the sync entrypoint.
[[changes]]
crate = \"zaino-state\"
kind = \"breaking\"
description = \"Replace sync().\"
";

    fn cycle(raw: &str) -> CycleId {
        CycleId::parse(raw).expect("valid cycle id")
    }

    #[test]
    fn consume_stamps_pending_files_and_preserves_their_body() {
        let store = Arc::new(MapChangesetStore::new());
        store
            .write(&slug("pr-1"), FILLED_WITH_COMMENT)
            .expect("seed one");
        store
            .write(&slug("pr-2"), "[empty]\nreason = \"comment-only\"\n")
            .expect("seed two");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let consumed = svc.consume(&cycle("cycle-1")).expect("consume succeeds");
        assert_eq!(as_strs(&consumed), ["pr-1", "pr-2"]);
        // Nothing was deleted — the ledger stays on disk.
        assert_eq!(slugs_in(&store), ["pr-1", "pr-2"]);

        // pr-1 gained the mark, kept its comment, and its body still parses.
        let raw = store.read(&slug("pr-1")).expect("read pr-1");
        assert!(raw.contains("# This PR replaces the sync entrypoint."));
        let stored = StoredChangeset::parse_toml(&raw).expect("stamped file still parses");
        assert_eq!(stored.consumed_in(), Some(&cycle("cycle-1")));
        assert_eq!(
            stored.body(),
            &Changeset::parse_toml(FILLED_WITH_COMMENT).expect("body")
        );
    }

    #[test]
    fn consume_preserves_a_changesets_id() {
        // Author an empty changeset through the service so it carries a real id,
        // then consume it and confirm the stamp left the id intact.
        let store = Arc::new(MapChangesetStore::new());
        let svc = service(store.clone(), vec![slug("wandering-quokka")]);
        let reason = Description::parse("Comment-only; no API change.").expect("non-empty");
        let written = svc
            .new(NewChangeset::Empty { reason })
            .expect("empty changeset should be created");

        svc.consume(&cycle("cycle-1")).expect("consume succeeds");

        let raw = store.read(&written).expect("read consumed file");
        let stored = StoredChangeset::parse_toml(&raw).expect("consumed file still parses");
        assert_eq!(stored.id(), Some(&uid(SAMPLE_UID)));
        assert_eq!(stored.consumed_in(), Some(&cycle("cycle-1")));
    }

    #[test]
    fn consume_appends_ids_to_the_ledger_skips_legacy_and_is_idempotent() {
        let store = Arc::new(MapChangesetStore::new());
        // An id-bearing changeset: its id must land in the ledger.
        let with_id = format!(
            "id = \"{SAMPLE_UID}\"\n[[changes]]\ncrate = \"zaino-state\"\nkind = \"fix\"\ndescription = \"x\"\n"
        );
        store
            .write(&slug("pr-1"), &with_id)
            .expect("seed id-bearing");
        // A legacy changeset with no id: stamped, but contributes no ledger row.
        store
            .write(&slug("pr-2"), FILLED_WITH_COMMENT)
            .expect("seed legacy");

        let ledger_store = Arc::new(MapConsumedLedgerStore::new());
        let svc = service_with_ledger(
            store.clone(),
            vec![slug("unused-source")],
            ledger_store.clone(),
        );

        let consumed = svc.consume(&cycle("cycle-1")).expect("consume");
        assert_eq!(as_strs(&consumed), ["pr-1", "pr-2"]);

        let ledger = ledger_store.read().expect("read ledger");
        // Only the id-bearing changeset produced a row (the legacy one is skipped).
        assert_eq!(ledger.len(), 1);
        assert!(ledger.contains(&uid(SAMPLE_UID)));
        let entry = ledger.entries().next().expect("one row");
        assert_eq!(entry.cycle(), &cycle("cycle-1"));
        assert_eq!(entry.slug(), Some("pr-1"));

        // Re-consuming finds nothing pending (both now marked) and leaves the
        // ledger untouched — idempotent.
        let again = svc.consume(&cycle("cycle-2")).expect("second consume");
        assert!(again.is_empty());
        assert_eq!(ledger_store.read().expect("read ledger").len(), 1);
    }

    #[test]
    fn consume_is_idempotent_on_already_consumed_files() {
        let store = Arc::new(MapChangesetStore::new());
        store
            .write(&slug("pr-1"), FILLED_WITH_COMMENT)
            .expect("seed");
        let svc = service(store.clone(), vec![slug("unused-source")]);

        let first = svc.consume(&cycle("cycle-1")).expect("first consume");
        assert_eq!(as_strs(&first), ["pr-1"]);
        let after_first = store.read(&slug("pr-1")).expect("read");

        // A second consume (even naming a different cycle) stamps nothing new
        // and leaves the already-consumed file byte-for-byte unchanged.
        let second = svc.consume(&cycle("cycle-2")).expect("second consume");
        assert!(second.is_empty());
        assert_eq!(store.read(&slug("pr-1")).expect("read"), after_first);
    }

    #[test]
    fn pending_excludes_consumed_changesets() {
        let store = Arc::new(MapChangesetStore::new());
        store
            .write(&slug("pending"), FILLED_WITH_COMMENT)
            .expect("seed pending");
        store
            .write(
                &slug("consumed"),
                "consumed_in = \"cycle-0\"\n[empty]\nreason = \"old\"\n",
            )
            .expect("seed consumed");
        let svc = service(store, vec![slug("unused-source")]);

        let pending = svc.pending().expect("pending listing");
        assert_eq!(as_strs(&pending), ["pending"]);
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
