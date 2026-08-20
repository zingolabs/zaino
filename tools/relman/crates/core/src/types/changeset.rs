use serde::{Deserialize, Serialize};

use crate::types::{
    ChangeEntry, ChangeKind, CrateName, CycleId, Description, InvalidChangeKind, InvalidCrateName,
    InvalidCycleId, InvalidSection, InvalidUid, Section, Uid,
};

/// The top-level TOML key that stamps a released changeset as consumed. Holds a
/// [`CycleId`] string (e.g. `"cycle-1"`). Exported so the format-preserving
/// stamp path shares one source of truth with the serde field below — a test
/// round-trips through both, so the two can never silently drift.
pub const CONSUMED_IN_KEY: &str = "consumed_in";

/// A parsed changeset file: the release-facing content of one PR.
///
/// Exactly one of two shapes, guaranteed by construction:
/// - [`WithChanges`](Changeset::WithChanges) — a non-empty list of
///   [`ChangeEntry`]s (the empty case is impossible to hold);
/// - [`Empty`](Changeset::Empty) — an escape hatch for a release-irrelevant PR,
///   carrying a non-empty reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changeset {
    /// One or more declared changes. Non-empty by construction.
    WithChanges(Vec<ChangeEntry>),
    /// No release-relevant change; a required reason records why.
    Empty {
        /// The auditable justification for an empty changeset.
        reason: Description,
    },
}

/// Everything that can go wrong parsing a changeset TOML document.
///
/// Parse-don't-validate: invalid input fails here, so holders of a
/// [`Changeset`] never re-check its invariants.
#[derive(Debug, thiserror::Error)]
pub enum ChangesetError {
    /// The bytes were not valid TOML for the expected schema.
    #[error("failed to parse changeset TOML")]
    Toml(#[source] toml::de::Error),
    /// Both a `[[changes]]` array and an `[empty]` table were present.
    #[error("a changeset must not declare both [[changes]] and [empty]")]
    BothChangesAndEmpty,
    /// The document declared nothing at all — neither a `[[changes]]` array nor
    /// an `[empty]` table, and no unrecognized content. This is the genuine
    /// "ran `changeset new` and haven't edited the template yet" state: a
    /// comments-only or whitespace-only file. Distinct from the malformed
    /// variants above/below so aggregation can *tolerate* it (skip with a
    /// warning) while `changeset check` flags it — an unfilled template is not
    /// yet a valid changeset, but neither is it a broken one.
    #[error("changeset is an unfilled template: declares neither [[changes]] nor [empty]")]
    Unfilled,
    /// A `changes` array was present but empty.
    #[error("[[changes]] is present but empty; use [empty] with a reason instead")]
    EmptyChanges,
    /// A change's `crate` was not a valid crate name.
    #[error("invalid crate name {value:?} in changeset entry")]
    InvalidCrate {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidCrateName,
    },
    /// A change's `kind` was not a known kind.
    #[error("invalid kind {value:?} in changeset entry")]
    InvalidKind {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidChangeKind,
    },
    /// A change's `section` override was not a known section.
    #[error("invalid section {value:?} in changeset entry")]
    InvalidSection {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidSection,
    },
    /// A change's `description` was empty.
    #[error("changeset entry for {crate_name:?} has an empty description")]
    EmptyDescription {
        /// The crate the offending entry named.
        crate_name: String,
    },
    /// An `[empty]` changeset had an empty `reason`.
    #[error("[empty] changeset has an empty reason")]
    EmptyReason,
    /// The optional [`consumed_in`](CONSUMED_IN_KEY) mark was present but not a
    /// valid [`CycleId`], so the provenance stamp cannot be trusted.
    #[error("invalid consumed_in cycle id {value:?} in changeset")]
    InvalidConsumedIn {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidCycleId,
    },
    /// The optional [`id`](StoredChangeset::id) was present but not a valid
    /// [`Uid`], so the changeset's stamped identity cannot be trusted.
    #[error("invalid id {value:?} in changeset")]
    InvalidUid {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidUid,
    },
}

impl Changeset {
    /// Construct a [`WithChanges`](Changeset::WithChanges), enforcing the
    /// non-empty invariant. Returns [`ChangesetError::EmptyChanges`] if the
    /// list is empty.
    pub fn with_changes(entries: Vec<ChangeEntry>) -> Result<Self, ChangesetError> {
        if entries.is_empty() {
            return Err(ChangesetError::EmptyChanges);
        }
        Ok(Self::WithChanges(entries))
    }

    /// Construct an [`Empty`](Changeset::Empty) from an already-validated
    /// non-empty reason.
    pub fn empty(reason: Description) -> Self {
        Self::Empty { reason }
    }

    /// Parse a changeset from its TOML representation, validating every field
    /// and enforcing the shape invariants.
    pub fn parse_toml(input: &str) -> Result<Self, ChangesetError> {
        let raw: RawChangeset = toml::from_str(input).map_err(ChangesetError::Toml)?;
        raw.into_changeset()
    }

    /// Serialize back to TOML such that
    /// [`parse_toml`](Changeset::parse_toml) round-trips to `self`.
    pub fn to_toml(&self) -> String {
        RawChangeset::from_changeset(self).to_toml()
    }
}

/// A changeset as it lives on disk: its immutable [`id`](StoredChangeset::id),
/// its release-facing [`Changeset`] body, and the optional
/// [`consumed_in`](CONSUMED_IN_KEY) provenance mark.
///
/// The `id` is a stable [`Uid`] assigned once at creation; it is `Option` so a
/// legacy or hand-written changeset that predates the field still parses (as
/// `None`) rather than erroring. Nothing consumes the id yet — it is baked in
/// now so identity is stable before real changesets ship.
///
/// The mark is orthogonal to the body shape — a `WithChanges` *or* an `Empty`
/// changeset can be consumed — so it lives here rather than polluting the pure
/// [`Changeset`] enum. A consumed changeset is one that a past release folded
/// in; it is left on disk as a ledger entry, and every version/changelog
/// derivation filters it out ([`consumed_in`](StoredChangeset::consumed_in) is
/// `Some`). A *pending* changeset has no mark and still contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredChangeset {
    id: Option<Uid>,
    consumed_in: Option<CycleId>,
    body: Changeset,
}

impl StoredChangeset {
    /// Wrap a body with its (optional) immutable id and consumed mark.
    pub fn new(id: Option<Uid>, consumed_in: Option<CycleId>, body: Changeset) -> Self {
        Self {
            id,
            consumed_in,
            body,
        }
    }

    /// Parse a stored changeset from its TOML representation: the optional
    /// [`id`](StoredChangeset::id) (validated through [`Uid`]), the release body
    /// (validated exactly as [`Changeset::parse_toml`]), and the optional
    /// [`consumed_in`](CONSUMED_IN_KEY) mark (validated through [`CycleId`]).
    pub fn parse_toml(input: &str) -> Result<Self, ChangesetError> {
        let raw: RawChangeset = toml::from_str(input).map_err(ChangesetError::Toml)?;
        let id = raw.id_parsed()?;
        let consumed_in = raw.consumed_in_parsed()?;
        let body = raw.into_changeset()?;
        Ok(Self {
            id,
            consumed_in,
            body,
        })
    }

    /// Read only the [`consumed_in`](CONSUMED_IN_KEY) mark, without validating
    /// the release body. Lets a caller decide pending-vs-consumed even for an
    /// unfilled template (whose body would otherwise fail to parse).
    pub fn consumed_marker(input: &str) -> Result<Option<CycleId>, ChangesetError> {
        let raw: RawChangeset = toml::from_str(input).map_err(ChangesetError::Toml)?;
        raw.consumed_in_parsed()
    }

    /// Read only the immutable [`id`](StoredChangeset::id), without validating
    /// the release body. Lets the consume path recover a changeset's identity for
    /// the ledger even when its body is an unfilled template (which would
    /// otherwise fail to parse). `None` for a legacy file that predates the field.
    pub fn id_marker(input: &str) -> Result<Option<Uid>, ChangesetError> {
        let raw: RawChangeset = toml::from_str(input).map_err(ChangesetError::Toml)?;
        raw.id_parsed()
    }

    /// The changeset's immutable identity, or `None` for a legacy file that
    /// predates the `id` field.
    pub fn id(&self) -> Option<&Uid> {
        self.id.as_ref()
    }

    /// The cycle that consumed this changeset, or `None` if it is still pending.
    pub fn consumed_in(&self) -> Option<&CycleId> {
        self.consumed_in.as_ref()
    }

    /// The release-facing body.
    pub fn body(&self) -> &Changeset {
        &self.body
    }

    /// Take the release-facing body, discarding the mark.
    pub fn into_body(self) -> Changeset {
        self.body
    }

    /// Serialize back to TOML such that [`parse_toml`](StoredChangeset::parse_toml)
    /// round-trips to `self`, id and mark included.
    pub fn to_toml(&self) -> String {
        let mut raw = RawChangeset::from_changeset(&self.body);
        raw.id = self.id.as_ref().map(|u| u.as_str().to_owned());
        raw.consumed_in = self.consumed_in.as_ref().map(|c| c.as_str().to_owned());
        raw.to_toml()
    }
}

/// The changeset document, mirrored for serde. `[[changes]]` deserializes as
/// the `changes` array; `[empty]` as the `empty` table.
///
/// Both fields are `Option` so an *absent* key is distinguishable from a
/// present-but-empty one — the shape rules turn on that distinction.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawChangeset {
    /// The immutable changeset identity. Optional (a legacy file may lack it)
    /// and serialized first so this bare key precedes any `[[changes]]`/`[empty]`
    /// table (a bare key after a table header would parse into that table, not
    /// the document root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// The provenance mark. Optional and serialized after `id` (but still before
    /// the body tables) so a consumed file's top-level keys precede any
    /// `[[changes]]`/`[empty]` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    consumed_in: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    changes: Option<Vec<RawChange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    empty: Option<RawEmpty>,
}

/// One `[[changes]]` entry, mirrored for serde.
#[derive(Debug, Deserialize, Serialize)]
struct RawChange {
    #[serde(rename = "crate")]
    crate_name: String,
    kind: String,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    migration: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    issues: Vec<String>,
}

/// The `[empty]` table, mirrored for serde.
#[derive(Debug, Deserialize, Serialize)]
struct RawEmpty {
    reason: String,
}

impl RawChangeset {
    /// Validate the raw `id` string through [`Uid`], if present.
    fn id_parsed(&self) -> Result<Option<Uid>, ChangesetError> {
        match &self.id {
            Some(raw) => Uid::parse(raw)
                .map(Some)
                .map_err(|source| ChangesetError::InvalidUid {
                    value: raw.clone(),
                    source,
                }),
            None => Ok(None),
        }
    }

    /// Validate the raw `consumed_in` string through [`CycleId`], if present.
    fn consumed_in_parsed(&self) -> Result<Option<CycleId>, ChangesetError> {
        match &self.consumed_in {
            Some(raw) => {
                CycleId::parse(raw)
                    .map(Some)
                    .map_err(|source| ChangesetError::InvalidConsumedIn {
                        value: raw.clone(),
                        source,
                    })
            }
            None => Ok(None),
        }
    }

    fn into_changeset(self) -> Result<Changeset, ChangesetError> {
        match (self.changes, self.empty) {
            (Some(_), Some(_)) => Err(ChangesetError::BothChangesAndEmpty),
            (None, None) => Err(ChangesetError::Unfilled),
            (Some(raw_changes), None) => {
                if raw_changes.is_empty() {
                    return Err(ChangesetError::EmptyChanges);
                }
                let mut entries = Vec::with_capacity(raw_changes.len());
                for raw in raw_changes {
                    entries.push(raw.into_entry()?);
                }
                Changeset::with_changes(entries)
            }
            (None, Some(raw_empty)) => {
                let reason = Description::parse(&raw_empty.reason)
                    .map_err(|_| ChangesetError::EmptyReason)?;
                Ok(Changeset::empty(reason))
            }
        }
    }

    fn from_changeset(changeset: &Changeset) -> Self {
        match changeset {
            Changeset::WithChanges(entries) => Self {
                id: None,
                consumed_in: None,
                changes: Some(entries.iter().map(RawChange::from_entry).collect()),
                empty: None,
            },
            Changeset::Empty { reason } => Self {
                id: None,
                consumed_in: None,
                changes: None,
                empty: Some(RawEmpty {
                    reason: reason.as_str().to_owned(),
                }),
            },
        }
    }

    fn to_toml(&self) -> String {
        // Serializing our own mirror of a fixed schema cannot fail: every
        // field is a plain string/array with no map-key or datetime hazard.
        toml::to_string(self).expect("changeset mirror is always serializable")
    }
}

impl RawChange {
    fn into_entry(self) -> Result<ChangeEntry, ChangesetError> {
        let crate_name =
            CrateName::parse(&self.crate_name).map_err(|source| ChangesetError::InvalidCrate {
                value: self.crate_name.clone(),
                source,
            })?;
        let kind = ChangeKind::parse(&self.kind).map_err(|source| ChangesetError::InvalidKind {
            value: self.kind.clone(),
            source,
        })?;
        let description = Description::parse(&self.description).map_err(|_| {
            ChangesetError::EmptyDescription {
                crate_name: self.crate_name.clone(),
            }
        })?;
        let section = match self.section {
            Some(raw) => Some(
                Section::parse(&raw)
                    .map_err(|source| ChangesetError::InvalidSection { value: raw, source })?,
            ),
            None => None,
        };
        Ok(ChangeEntry::new(
            crate_name,
            kind,
            description,
            section,
            self.migration,
            self.issues,
        ))
    }

    fn from_entry(entry: &ChangeEntry) -> Self {
        Self {
            crate_name: entry.crate_name().as_str().to_owned(),
            kind: entry.kind().as_str().to_owned(),
            description: entry.description().as_str().to_owned(),
            section: entry.section().map(|s| s.as_str().to_owned()),
            migration: entry.migration().map(str::to_owned),
            issues: entry.issues().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The changeset-format decision record's worked example (PR #1501):
    /// a breaking change with a migration note, plus a fix.
    const MULTI_ENTRY: &str = r##"
[[changes]]
crate = "zaino-state"
kind = "breaking"
description = "Replace the `sync()` entrypoint with `sync_with(SyncMode)`."
migration = "Call `sync_with(SyncMode::Serial)` for the previous behaviour."
issues = ["#987", "#990"]

[[changes]]
crate = "zaino-state"
kind = "fix"
description = "Stop double-counting orphaned blocks in the tip height gauge."
"##;

    fn parse_ok(input: &str) -> Changeset {
        Changeset::parse_toml(input).expect("should parse")
    }

    #[test]
    fn parses_multi_entry_changeset() {
        let Changeset::WithChanges(entries) = parse_ok(MULTI_ENTRY) else {
            panic!("expected WithChanges");
        };
        assert_eq!(entries.len(), 2);

        let breaking = &entries[0];
        assert_eq!(breaking.crate_name().as_str(), "zaino-state");
        assert_eq!(breaking.kind(), ChangeKind::Breaking);
        // breaking defaults to the Changed section.
        assert_eq!(breaking.effective_section(), Section::Changed);
        assert_eq!(
            breaking.migration(),
            Some("Call `sync_with(SyncMode::Serial)` for the previous behaviour.")
        );
        assert_eq!(breaking.issues(), ["#987", "#990"]);

        let fix = &entries[1];
        assert_eq!(fix.kind(), ChangeKind::Fix);
        assert_eq!(fix.effective_section(), Section::Fixed);
        assert_eq!(fix.migration(), None);
        assert!(fix.issues().is_empty());
    }

    #[test]
    fn parses_empty_changeset() {
        let input = r#"
[empty]
reason = "Comment-only fix in zaino-state; no behavioural or API change."
"#;
        let Changeset::Empty { reason } = parse_ok(input) else {
            panic!("expected Empty");
        };
        assert_eq!(
            reason.as_str(),
            "Comment-only fix in zaino-state; no behavioural or API change."
        );
    }

    #[test]
    fn honours_explicit_section_override() {
        let input = r#"
[[changes]]
crate = "zaino-state"
kind = "breaking"
description = "Remove the deprecated `legacy_sync` API."
section = "removed"
"#;
        let Changeset::WithChanges(entries) = parse_ok(input) else {
            panic!("expected WithChanges");
        };
        assert_eq!(entries[0].section(), Some(Section::Removed));
        assert_eq!(entries[0].effective_section(), Section::Removed);
    }

    #[test]
    fn rejects_both_changes_and_empty() {
        let input = r#"
[[changes]]
crate = "zaino-state"
kind = "fix"
description = "A fix."

[empty]
reason = "nope"
"#;
        assert!(matches!(
            Changeset::parse_toml(input),
            Err(ChangesetError::BothChangesAndEmpty)
        ));
    }

    #[test]
    fn comments_only_is_unfilled() {
        // The commented scaffold `changeset new` writes: inert until edited.
        assert!(matches!(
            Changeset::parse_toml("# just a comment\n"),
            Err(ChangesetError::Unfilled)
        ));
    }

    #[test]
    fn whitespace_only_is_unfilled() {
        assert!(matches!(
            Changeset::parse_toml("   \n\n\t\n"),
            Err(ChangesetError::Unfilled)
        ));
    }

    #[test]
    fn empty_string_is_unfilled() {
        assert!(matches!(
            Changeset::parse_toml(""),
            Err(ChangesetError::Unfilled)
        ));
    }

    #[test]
    fn typoed_table_is_malformed_not_unfilled() {
        // A typo'd top-level table (`[[chagnes]]` for `[[changes]]`) has real
        // content that violates the schema. `deny_unknown_fields` makes it a
        // parse error, NOT the unfilled-template state — so it still hard-errors
        // through the aggregation path instead of being silently skipped.
        let input = r#"
[[chagnes]]
crate = "zaino-state"
kind = "fix"
description = "A change."
"#;
        let parsed = Changeset::parse_toml(input);
        assert!(
            matches!(parsed, Err(ChangesetError::Toml(_))),
            "expected a Toml parse error, got {parsed:?}"
        );
    }

    #[test]
    fn rejects_empty_changes_array() {
        assert!(matches!(
            Changeset::parse_toml("changes = []\n"),
            Err(ChangesetError::EmptyChanges)
        ));
    }

    #[test]
    fn rejects_unknown_kind() {
        let input = r#"
[[changes]]
crate = "zaino-state"
kind = "major"
description = "A change."
"#;
        assert!(matches!(
            Changeset::parse_toml(input),
            Err(ChangesetError::InvalidKind { value, .. }) if value == "major"
        ));
    }

    #[test]
    fn rejects_unknown_section() {
        let input = r#"
[[changes]]
crate = "zaino-state"
kind = "fix"
description = "A change."
section = "performance"
"#;
        assert!(matches!(
            Changeset::parse_toml(input),
            Err(ChangesetError::InvalidSection { value, .. }) if value == "performance"
        ));
    }

    #[test]
    fn rejects_empty_description() {
        let input = r#"
[[changes]]
crate = "zaino-state"
kind = "fix"
description = "   "
"#;
        assert!(matches!(
            Changeset::parse_toml(input),
            Err(ChangesetError::EmptyDescription { crate_name }) if crate_name == "zaino-state"
        ));
    }

    #[test]
    fn rejects_empty_reason() {
        let input = r#"
[empty]
reason = "   "
"#;
        assert!(matches!(
            Changeset::parse_toml(input),
            Err(ChangesetError::EmptyReason)
        ));
    }

    #[test]
    fn rejects_invalid_crate_name() {
        let input = r#"
[[changes]]
crate = "zaino.state"
kind = "fix"
description = "A change."
"#;
        assert!(matches!(
            Changeset::parse_toml(input),
            Err(ChangesetError::InvalidCrate { value, .. }) if value == "zaino.state"
        ));
    }

    #[test]
    fn round_trips_with_changes() {
        let original = parse_ok(MULTI_ENTRY);
        let reparsed = parse_ok(&original.to_toml());
        assert_eq!(original, reparsed);
    }

    #[test]
    fn round_trips_empty() {
        let original = Changeset::empty(Description::parse("release-irrelevant").expect("ok"));
        let reparsed = parse_ok(&original.to_toml());
        assert_eq!(original, reparsed);
    }

    #[test]
    fn with_changes_rejects_empty_vec() {
        assert!(matches!(
            Changeset::with_changes(Vec::new()),
            Err(ChangesetError::EmptyChanges)
        ));
    }

    fn cycle(raw: &str) -> CycleId {
        CycleId::parse(raw).expect("valid cycle id")
    }

    #[test]
    fn stored_changeset_parses_pending_body_with_no_mark() {
        let stored = StoredChangeset::parse_toml(MULTI_ENTRY).expect("should parse");
        assert!(stored.consumed_in().is_none());
        let Changeset::WithChanges(entries) = stored.body() else {
            panic!("expected WithChanges");
        };
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn stored_changeset_parses_consumed_mark() {
        let input = format!("consumed_in = \"cycle-1\"\n{MULTI_ENTRY}");
        let stored = StoredChangeset::parse_toml(&input).expect("should parse");
        assert_eq!(stored.consumed_in(), Some(&cycle("cycle-1")));
        // The mark leaves the body untouched.
        assert_eq!(stored.body(), &parse_ok(MULTI_ENTRY));
    }

    #[test]
    fn consumed_mark_is_orthogonal_to_the_empty_body() {
        let input = "consumed_in = \"cycle-2\"\n[empty]\nreason = \"comment-only\"\n";
        let stored = StoredChangeset::parse_toml(input).expect("should parse");
        assert_eq!(stored.consumed_in(), Some(&cycle("cycle-2")));
        assert!(matches!(stored.body(), Changeset::Empty { .. }));
    }

    #[test]
    fn stored_changeset_rejects_invalid_consumed_mark() {
        let input = format!("consumed_in = \"Cycle_1\"\n{MULTI_ENTRY}");
        assert!(matches!(
            StoredChangeset::parse_toml(&input),
            Err(ChangesetError::InvalidConsumedIn { value, .. }) if value == "Cycle_1"
        ));
    }

    #[test]
    fn consumed_marker_reads_the_mark_without_validating_the_body() {
        // An unfilled template (no [[changes]]/[empty]) still yields its mark.
        let input = "consumed_in = \"cycle-3\"\n# nothing else\n";
        assert_eq!(
            StoredChangeset::consumed_marker(input).expect("marker parses"),
            Some(cycle("cycle-3"))
        );
        // A pending template has no mark.
        assert_eq!(
            StoredChangeset::consumed_marker("# just a comment\n").expect("marker parses"),
            None
        );
    }

    #[test]
    fn stored_changeset_round_trips_with_and_without_mark() {
        let pending = StoredChangeset::new(None, None, parse_ok(MULTI_ENTRY));
        let reparsed = StoredChangeset::parse_toml(&pending.to_toml()).expect("reparse");
        assert_eq!(pending, reparsed);

        let consumed = StoredChangeset::new(None, Some(cycle("cycle-1")), parse_ok(MULTI_ENTRY));
        let reparsed = StoredChangeset::parse_toml(&consumed.to_toml()).expect("reparse");
        assert_eq!(consumed, reparsed);
        assert_eq!(reparsed.consumed_in(), Some(&cycle("cycle-1")));
    }

    fn uid(raw: &str) -> Uid {
        Uid::parse(raw).expect("valid test uid")
    }

    /// A canonical UUIDv7 in hyphenated lowercase form.
    const SAMPLE_UID: &str = "018f4e0a-7b2c-7c3d-8e4f-1a2b3c4d5e6f";

    #[test]
    fn stored_changeset_round_trips_with_id() {
        let with_id = StoredChangeset::new(Some(uid(SAMPLE_UID)), None, parse_ok(MULTI_ENTRY));
        let reparsed = StoredChangeset::parse_toml(&with_id.to_toml()).expect("reparse");
        assert_eq!(with_id, reparsed);
        assert_eq!(reparsed.id(), Some(&uid(SAMPLE_UID)));
    }

    #[test]
    fn stored_changeset_emits_id_before_consumed_in_and_body() {
        let stored = StoredChangeset::new(
            Some(uid(SAMPLE_UID)),
            Some(cycle("cycle-1")),
            parse_ok(MULTI_ENTRY),
        );
        let toml = stored.to_toml();
        let id_at = toml.find("id = ").expect("id key present");
        let consumed_at = toml
            .find("consumed_in = ")
            .expect("consumed_in key present");
        let body_at = toml.find("[[changes]]").expect("body table present");
        assert!(id_at < consumed_at, "id must precede consumed_in");
        assert!(
            consumed_at < body_at,
            "bare keys must precede the array-of-tables"
        );
    }

    #[test]
    fn id_marker_reads_the_id_without_validating_the_body() {
        // An unfilled template (no [[changes]]/[empty]) still yields its id.
        let input = format!("id = \"{SAMPLE_UID}\"\n# nothing else\n");
        assert_eq!(
            StoredChangeset::id_marker(&input).expect("marker parses"),
            Some(uid(SAMPLE_UID))
        );
        // A legacy file with no id yields None.
        assert_eq!(
            StoredChangeset::id_marker("# just a comment\n").expect("marker parses"),
            None
        );
    }

    #[test]
    fn stored_changeset_tolerates_a_missing_id() {
        // A legacy file with no `id` still parses, yielding `None`.
        let stored = StoredChangeset::parse_toml(MULTI_ENTRY).expect("should parse");
        assert!(stored.id().is_none());
    }

    #[test]
    fn stored_changeset_rejects_a_malformed_id() {
        let input = format!("id = \"not-a-uuid\"\n{MULTI_ENTRY}");
        assert!(matches!(
            StoredChangeset::parse_toml(&input),
            Err(ChangesetError::InvalidUid { value, .. }) if value == "not-a-uuid"
        ));
    }
}
