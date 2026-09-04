use crate::types::{ChangeKind, CrateName, Description, Section};

/// One `[[changes]]` entry: a single semantic change to one governed crate.
///
/// Every field is already validated — `crate_name` is a parsed [`CrateName`],
/// `kind` a [`ChangeKind`], `description` a non-empty [`Description`]. `section`
/// is an optional override; when absent, [`effective_section`](
/// ChangeEntry::effective_section) derives it from the kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEntry {
    crate_name: CrateName,
    kind: ChangeKind,
    description: Description,
    section: Option<Section>,
    migration: Option<String>,
    issues: Vec<String>,
}

impl ChangeEntry {
    /// Construct an entry from already-validated parts.
    pub fn new(
        crate_name: CrateName,
        kind: ChangeKind,
        description: Description,
        section: Option<Section>,
        migration: Option<String>,
        issues: Vec<String>,
    ) -> Self {
        Self {
            crate_name,
            kind,
            description,
            section,
            migration,
            issues,
        }
    }

    /// The governed crate this change targets.
    pub fn crate_name(&self) -> &CrateName {
        &self.crate_name
    }

    /// The semantic kind of the change.
    pub fn kind(&self) -> ChangeKind {
        self.kind
    }

    /// The changelog line.
    pub fn description(&self) -> &Description {
        &self.description
    }

    /// The explicit section override, if any.
    pub fn section(&self) -> Option<Section> {
        self.section
    }

    /// Migration/upgrade notes, if any (expected on `breaking`).
    pub fn migration(&self) -> Option<&str> {
        self.migration.as_deref()
    }

    /// Additional issue references (e.g. `"#987"`).
    pub fn issues(&self) -> &[String] {
        &self.issues
    }

    /// The section this entry renders under: the explicit override if present,
    /// otherwise the default derived from the kind.
    pub fn effective_section(&self) -> Section {
        self.section
            .unwrap_or_else(|| Section::default_for(self.kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ChangeKind, section: Option<Section>) -> ChangeEntry {
        ChangeEntry::new(
            CrateName::parse("zaino-state").expect("valid name"),
            kind,
            Description::parse("a change").expect("non-empty"),
            section,
            None,
            Vec::new(),
        )
    }

    #[test]
    fn effective_section_defaults_from_kind() {
        assert_eq!(
            entry(ChangeKind::Feature, None).effective_section(),
            Section::Added
        );
        assert_eq!(
            entry(ChangeKind::Breaking, None).effective_section(),
            Section::Changed
        );
    }

    #[test]
    fn effective_section_honours_override() {
        assert_eq!(
            entry(ChangeKind::Feature, Some(Section::Removed)).effective_section(),
            Section::Removed
        );
    }
}
