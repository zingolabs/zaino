use crate::types::Tag;

/// An ordered set of git tags to apply — the plan `relman tags` emits for CI to
/// `git tag` verbatim.
///
/// The order is meaningful only as a stable rendering order (cycle tag first,
/// then per-crate provenance tags in config order); git applies them
/// independently.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TagPlan {
    tags: Vec<Tag>,
}

impl TagPlan {
    /// Construct from the ordered list of tags.
    pub fn new(tags: Vec<Tag>) -> Self {
        Self { tags }
    }

    /// The tags, in application/render order.
    pub fn tags(&self) -> &[Tag] {
        &self.tags
    }

    /// Whether the plan is empty.
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::{CycleId, Tag};

    #[test]
    fn preserves_order_and_reports_len() {
        let cycle = CycleId::parse("2026-08-15").expect("valid");
        let plan = TagPlan::new(vec![Tag::cycle(&cycle), Tag::cycle_rc(&cycle, 1)]);
        assert!(!plan.is_empty());
        assert_eq!(plan.tags().len(), 2);
        assert_eq!(plan.tags()[0].as_str(), "cycle-2026-08-15");
        assert_eq!(plan.tags()[1].as_str(), "cycle-2026-08-15-rc.1");
    }

    #[test]
    fn default_is_empty() {
        assert!(TagPlan::default().is_empty());
    }
}
