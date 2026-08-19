use std::cmp::Ordering;

/// The semantic intent of a single change, as declared by a contributor.
///
/// This is **not** a literal semver bump: CI maps a `kind` to a bump per
/// crate, applying the pre-1.0 relaxation. See the changeset-format decision
/// record.
///
/// The kinds carry a severity ordering (`Breaking > Feature > Fix > Internal`)
/// so aggregation can resolve a crate's bump as "highest kind wins".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Backward-incompatible change to a governed public interface.
    Breaking,
    /// Backward-compatible addition.
    Feature,
    /// Backward-compatible bug/perf fix.
    Fix,
    /// No externally observable contract change (refactor, internal).
    Internal,
}

/// Why a string was rejected as a [`ChangeKind`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown change kind {found:?} \
     (expected one of: breaking, feature, fix, internal)"
)]
pub struct InvalidChangeKind {
    /// The unrecognised input.
    pub found: String,
}

impl ChangeKind {
    /// Parse the lowercase wire name of a kind.
    pub fn parse(raw: &str) -> Result<Self, InvalidChangeKind> {
        match raw {
            "breaking" => Ok(Self::Breaking),
            "feature" => Ok(Self::Feature),
            "fix" => Ok(Self::Fix),
            "internal" => Ok(Self::Internal),
            other => Err(InvalidChangeKind {
                found: other.to_owned(),
            }),
        }
    }

    /// The lowercase wire name of this kind.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Breaking => "breaking",
            Self::Feature => "feature",
            Self::Fix => "fix",
            Self::Internal => "internal",
        }
    }

    /// Severity rank, higher is more severe. Drives "highest kind wins"
    /// aggregation: `Breaking > Feature > Fix > Internal`.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Breaking => 3,
            Self::Feature => 2,
            Self::Fix => 1,
            Self::Internal => 0,
        }
    }
}

impl Ord for ChangeKind {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for ChangeKind {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_as_str() {
        for kind in [
            ChangeKind::Breaking,
            ChangeKind::Feature,
            ChangeKind::Fix,
            ChangeKind::Internal,
        ] {
            assert_eq!(ChangeKind::parse(kind.as_str()), Ok(kind));
        }
    }

    #[test]
    fn rejects_unknown_kind() {
        assert_eq!(
            ChangeKind::parse("major"),
            Err(InvalidChangeKind {
                found: "major".to_owned()
            })
        );
    }

    #[test]
    fn rejects_wrong_case() {
        // Wire names are lowercase; anything else is unknown.
        assert!(ChangeKind::parse("Breaking").is_err());
    }

    #[test]
    fn severity_ordering_is_breaking_highest() {
        assert!(ChangeKind::Breaking > ChangeKind::Feature);
        assert!(ChangeKind::Feature > ChangeKind::Fix);
        assert!(ChangeKind::Fix > ChangeKind::Internal);

        // The max of a mixed set is the most severe kind.
        let max = [ChangeKind::Fix, ChangeKind::Breaking, ChangeKind::Internal]
            .into_iter()
            .max();
        assert_eq!(max, Some(ChangeKind::Breaking));
    }
}
