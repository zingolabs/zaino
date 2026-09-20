//! Rendering an error with its full cause chain, for coherent failure reporting.

use std::error::Error;

/// Render `err` and every error in its `source` chain as one line:
/// `"top: cause: root"`.
///
/// The supervision boundary uses this both to log a failed component's cause and
/// to record it on the component's [`ComponentStatus::reason`], so a single
/// failure surfaces its whole chain — not just the outermost variant, which
/// alone rarely names the root.
///
/// [`ComponentStatus::reason`]: crate::ComponentStatus::reason
pub fn error_chain(err: &dyn Error) -> String {
    let mut chain = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::error_chain;

    #[derive(Debug, thiserror::Error)]
    #[error("root cause")]
    struct Root;

    #[derive(Debug, thiserror::Error)]
    #[error("middle failed")]
    struct Middle(#[source] Root);

    #[derive(Debug, thiserror::Error)]
    #[error("top failed")]
    struct Top(#[source] Middle);

    #[test]
    fn joins_the_whole_source_chain() {
        assert_eq!(
            error_chain(&Top(Middle(Root))),
            "top failed: middle failed: root cause"
        );
    }

    #[test]
    fn a_lone_error_is_just_itself() {
        assert_eq!(error_chain(&Root), "root cause");
    }
}
