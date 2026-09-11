use crate::types::{Bump, CrateName, Version};

/// One crate's derived version bump: where it is now, where it goes, at what
/// level, and the human reasons why.
///
/// Reasons are operator-facing strings: direct changeset descriptions first,
/// then any transitive-bump explanations (e.g. a dependency whose new version
/// escaped this crate's declared requirement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateBump {
    crate_name: CrateName,
    current: Version,
    next: Version,
    bump: Bump,
    reasons: Vec<String>,
}

impl CrateBump {
    /// Construct from already-computed parts. `next` is expected to equal
    /// `bump.apply(&current)`; the deriving service guarantees this.
    pub fn new(
        crate_name: CrateName,
        current: Version,
        next: Version,
        bump: Bump,
        reasons: Vec<String>,
    ) -> Self {
        Self {
            crate_name,
            current,
            next,
            bump,
            reasons,
        }
    }

    /// The crate this bump applies to.
    pub fn crate_name(&self) -> &CrateName {
        &self.crate_name
    }

    /// The crate's current (last-released) version.
    pub fn current(&self) -> &Version {
        &self.current
    }

    /// The crate's next version after applying the bump.
    pub fn next(&self) -> &Version {
        &self.next
    }

    /// The bump level.
    pub fn bump(&self) -> Bump {
        self.bump
    }

    /// The human reasons for this bump, in render order (direct first, then
    /// transitive).
    pub fn reasons(&self) -> &[String] {
        &self.reasons
    }
}

/// The aggregated per-crate version derivation: every crate that bumps, in
/// `relman.toml` target order.
///
/// Only crates that actually bump appear; a crate with no direct changeset and
/// no transitive crossing is absent (its version is unchanged).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BumpTable {
    bumps: Vec<CrateBump>,
}

impl BumpTable {
    /// Construct from the ordered list of bumps (config-target order).
    pub fn new(bumps: Vec<CrateBump>) -> Self {
        Self { bumps }
    }

    /// The bumps, in target order.
    pub fn bumps(&self) -> &[CrateBump] {
        &self.bumps
    }

    /// Whether no crate bumps.
    pub fn is_empty(&self) -> bool {
        self.bumps.is_empty()
    }

    /// How many crates bump.
    pub fn len(&self) -> usize {
        self.bumps.len()
    }

    /// Look up a crate's bump by name.
    pub fn get(&self, name: &CrateName) -> Option<&CrateBump> {
        self.bumps.iter().find(|b| b.crate_name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crate_name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    fn sample() -> BumpTable {
        BumpTable::new(vec![
            CrateBump::new(
                crate_name("zaino-state"),
                version("0.3.1"),
                version("0.4.0"),
                Bump::Minor,
                vec!["a breaking change".to_owned()],
            ),
            CrateBump::new(
                crate_name("zainod"),
                version("0.4.3"),
                version("0.4.4"),
                Bump::Patch,
                vec![
                    "dependency `zaino-state` 0.3.1→0.4.0 crossed the requirement `^0.3`"
                        .to_owned(),
                ],
            ),
        ])
    }

    #[test]
    fn get_finds_by_name_and_preserves_order() {
        let table = sample();
        assert_eq!(table.len(), 2);
        assert!(!table.is_empty());
        // Order is the insertion (config) order.
        assert_eq!(table.bumps()[0].crate_name().as_str(), "zaino-state");
        assert_eq!(table.bumps()[1].crate_name().as_str(), "zainod");

        let state = table.get(&crate_name("zaino-state")).expect("present");
        assert_eq!(state.next(), &version("0.4.0"));
        assert_eq!(state.bump(), Bump::Minor);

        assert!(table.get(&crate_name("zaino-proto")).is_none());
    }

    #[test]
    fn empty_table_reports_empty() {
        assert!(BumpTable::default().is_empty());
    }
}
