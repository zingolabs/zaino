/// A non-empty piece of operator-facing text (a changelog line or an
/// empty-changeset reason).
///
/// Invariant, enforced once at [`parse`](Description::parse): the text is
/// non-empty after trimming surrounding whitespace. The stored value is the
/// trimmed text, so consumers never see leading/trailing padding and the
/// invariant is idempotent under re-parsing (which keeps round-trips stable).
/// Interior whitespace, including newlines, is preserved — multiline
/// descriptions are allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description(String);

/// Why a string was rejected as a [`Description`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("description is empty")]
pub struct EmptyDescription;

impl Description {
    /// Parse text into a description, rejecting empty/whitespace-only input.
    pub fn parse(raw: &str) -> Result<Self, EmptyDescription> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(EmptyDescription);
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Borrow the trimmed text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_trims_text() {
        let desc = Description::parse("  Add a parallel sync mode.  ").expect("non-empty");
        assert_eq!(desc.as_str(), "Add a parallel sync mode.");
    }

    #[test]
    fn preserves_interior_newlines() {
        let desc = Description::parse("line one\nline two").expect("non-empty");
        assert_eq!(desc.as_str(), "line one\nline two");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(Description::parse(""), Err(EmptyDescription));
    }

    #[test]
    fn rejects_whitespace_only() {
        assert_eq!(Description::parse("   \n\t "), Err(EmptyDescription));
    }
}
