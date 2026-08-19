use crate::types::ChangeKind;

/// A Keep-a-Changelog section heading a change is rendered under.
///
/// A change's section defaults from its [`ChangeKind`] (see
/// [`default_for`](Section::default_for)); an author may override it with an
/// explicit `section` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// New capabilities.
    Added,
    /// Changes to existing behaviour.
    Changed,
    /// Bug fixes.
    Fixed,
    /// Removed capabilities.
    Removed,
    /// Security-relevant changes.
    Security,
    /// Soon-to-be-removed capabilities.
    Deprecated,
    /// Non-user-facing changes (refactors); rendered apart or omitted.
    Internal,
}

/// Why a string was rejected as a [`Section`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown changelog section {found:?} (expected one of: \
     added, changed, fixed, removed, security, deprecated, internal)"
)]
pub struct InvalidSection {
    /// The unrecognised input.
    pub found: String,
}

impl Section {
    /// Parse the lowercase wire name of a section.
    pub fn parse(raw: &str) -> Result<Self, InvalidSection> {
        match raw {
            "added" => Ok(Self::Added),
            "changed" => Ok(Self::Changed),
            "fixed" => Ok(Self::Fixed),
            "removed" => Ok(Self::Removed),
            "security" => Ok(Self::Security),
            "deprecated" => Ok(Self::Deprecated),
            "internal" => Ok(Self::Internal),
            other => Err(InvalidSection {
                found: other.to_owned(),
            }),
        }
    }

    /// The title-case Keep-a-Changelog heading for this section, as rendered
    /// under a version's `### ` sub-heading (e.g. `Added`, `Fixed`).
    pub fn heading(&self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Changed => "Changed",
            Self::Fixed => "Fixed",
            Self::Removed => "Removed",
            Self::Security => "Security",
            Self::Deprecated => "Deprecated",
            Self::Internal => "Internal",
        }
    }

    /// The lowercase wire name of this section.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Changed => "changed",
            Self::Fixed => "fixed",
            Self::Removed => "removed",
            Self::Security => "security",
            Self::Deprecated => "deprecated",
            Self::Internal => "internal",
        }
    }

    /// The default section for a change of the given kind, per the
    /// changeset-format decision record: `feature → Added`, `fix → Fixed`,
    /// `breaking → Changed`, `internal → Internal`.
    pub fn default_for(kind: ChangeKind) -> Section {
        match kind {
            ChangeKind::Feature => Section::Added,
            ChangeKind::Fix => Section::Fixed,
            ChangeKind::Breaking => Section::Changed,
            ChangeKind::Internal => Section::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_as_str() {
        for section in [
            Section::Added,
            Section::Changed,
            Section::Fixed,
            Section::Removed,
            Section::Security,
            Section::Deprecated,
            Section::Internal,
        ] {
            assert_eq!(Section::parse(section.as_str()), Ok(section));
        }
    }

    #[test]
    fn rejects_unknown_section() {
        assert_eq!(
            Section::parse("performance"),
            Err(InvalidSection {
                found: "performance".to_owned()
            })
        );
    }

    #[test]
    fn heading_is_title_case() {
        assert_eq!(Section::Added.heading(), "Added");
        assert_eq!(Section::Internal.heading(), "Internal");
        assert_eq!(Section::Deprecated.heading(), "Deprecated");
    }

    #[test]
    fn default_for_maps_every_kind() {
        assert_eq!(Section::default_for(ChangeKind::Feature), Section::Added);
        assert_eq!(Section::default_for(ChangeKind::Fix), Section::Fixed);
        assert_eq!(Section::default_for(ChangeKind::Breaking), Section::Changed);
        assert_eq!(
            Section::default_for(ChangeKind::Internal),
            Section::Internal
        );
    }
}
