use crate::types::{CrateName, Version};

/// The dependency-ordered list of crates to publish, each at its next version.
///
/// Every crate appears **after** all governed crates it depends on that also
/// publish this cycle, so a `cargo publish` walk over
/// [`entries`](PublishPlan::entries) never hits an unpublished internal
/// dependency. Only crates that bump appear — unchanged crates are already on
/// crates.io and are skipped upstream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PublishPlan {
    entries: Vec<(CrateName, Version)>,
}

impl PublishPlan {
    /// Construct from the already-ordered `(crate, next-version)` pairs.
    pub fn new(entries: Vec<(CrateName, Version)>) -> Self {
        Self { entries }
    }

    /// The `(crate, next-version)` pairs, in publish (dependency) order.
    pub fn entries(&self) -> &[(CrateName, Version)] {
        &self.entries
    }

    /// Whether nothing publishes.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    #[test]
    fn preserves_publish_order() {
        let plan = PublishPlan::new(vec![
            (name("zaino-state"), version("0.4.0")),
            (name("zainod"), version("0.5.0")),
        ]);
        assert!(!plan.is_empty());
        assert_eq!(plan.entries().len(), 2);
        assert_eq!(plan.entries()[0].0.as_str(), "zaino-state");
        assert_eq!(plan.entries()[1].0.as_str(), "zainod");
    }

    #[test]
    fn default_is_empty() {
        assert!(PublishPlan::default().is_empty());
    }
}
