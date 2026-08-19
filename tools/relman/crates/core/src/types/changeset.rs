use serde::{Deserialize, Serialize};

use crate::types::{
    ChangeEntry, ChangeKind, CrateName, Description, InvalidChangeKind, InvalidCrateName,
    InvalidSection, Section,
};

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

/// The changeset document, mirrored for serde. `[[changes]]` deserializes as
/// the `changes` array; `[empty]` as the `empty` table.
///
/// Both fields are `Option` so an *absent* key is distinguishable from a
/// present-but-empty one — the shape rules turn on that distinction.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawChangeset {
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
                changes: Some(entries.iter().map(RawChange::from_entry).collect()),
                empty: None,
            },
            Changeset::Empty { reason } => Self {
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
}
