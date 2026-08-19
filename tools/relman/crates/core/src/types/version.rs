use std::fmt;

/// A crate's semantic version.
///
/// A thin newtype over [`semver::Version`], so the rest of relman speaks in a
/// domain type rather than a foreign one. Parse-don't-validate: once you hold a
/// `Version`, the string was a well-formed semver.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(semver::Version);

/// Why a string was rejected as a [`Version`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid semantic version {value:?}")]
pub struct InvalidVersion {
    /// The unparseable input.
    pub value: String,
    /// The underlying semver parse error, rendered.
    pub reason: String,
}

impl Version {
    /// Parse a semantic version string (`MAJOR.MINOR.PATCH[-pre][+build]`).
    pub fn parse(raw: &str) -> Result<Self, InvalidVersion> {
        semver::Version::parse(raw)
            .map(Self)
            .map_err(|err| InvalidVersion {
                value: raw.to_owned(),
                reason: err.to_string(),
            })
    }

    /// Wrap an already-constructed [`semver::Version`] (e.g. from
    /// `cargo metadata`, which parsed it for us).
    pub fn from_semver(version: semver::Version) -> Self {
        Self(version)
    }

    /// Borrow the underlying [`semver::Version`].
    pub fn as_semver(&self) -> &semver::Version {
        &self.0
    }

    /// Whether this version is in the pre-1.0 (`0.y.z`) phase, which governs the
    /// [pre-1.0 relaxation](crate::types::Bump::from_kind) of the kind→bump map.
    pub fn is_pre_1_0(&self) -> bool {
        self.0.major == 0
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays_round_trip() {
        for raw in ["0.3.1", "1.2.0", "0.0.0", "2.0.0-rc.1", "1.4.2+build.7"] {
            let version = Version::parse(raw).expect("valid semver");
            assert_eq!(version.to_string(), raw);
        }
    }

    #[test]
    fn rejects_non_semver() {
        for raw in ["", "1", "1.2", "not-a-version", "0.1.x"] {
            assert!(
                Version::parse(raw).is_err(),
                "expected {raw:?} to be invalid"
            );
        }
    }

    #[test]
    fn pre_1_0_detects_zero_major() {
        assert!(Version::parse("0.3.1").expect("valid").is_pre_1_0());
        assert!(Version::parse("0.0.1").expect("valid").is_pre_1_0());
        assert!(!Version::parse("1.0.0").expect("valid").is_pre_1_0());
        assert!(!Version::parse("2.3.4").expect("valid").is_pre_1_0());
    }

    #[test]
    fn ordering_is_semver_ordering() {
        let lo = Version::parse("0.3.1").expect("valid");
        let hi = Version::parse("0.4.0").expect("valid");
        assert!(lo < hi);
    }
}
