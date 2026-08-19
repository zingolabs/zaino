use crate::types::{ChangeKind, Version};

/// A literal semver bump level, derived from a [`ChangeKind`] and a crate's
/// current [`Version`].
///
/// Ordered `Major > Minor > Patch`, so aggregation and transitive bumps can
/// take the strongest bump with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bump {
    /// A patch bump: `M.m.p → M.m.(p+1)`.
    Patch,
    /// A minor bump: `M.m.p → M.(m+1).0`.
    Minor,
    /// A major bump: `M.m.p → (M+1).0.0`.
    Major,
}

impl Bump {
    /// Apply this bump to `current`, returning the next version.
    ///
    /// Bumping a component resets the lower ones to `0` and drops any
    /// pre-release / build metadata (a released version carries none), matching
    /// the standard semver bump semantics.
    pub fn apply(&self, current: &Version) -> Version {
        let v = current.as_semver();
        let next = match self {
            Self::Major => semver::Version::new(v.major + 1, 0, 0),
            Self::Minor => semver::Version::new(v.major, v.minor + 1, 0),
            Self::Patch => semver::Version::new(v.major, v.minor, v.patch + 1),
        };
        Version::from_semver(next)
    }

    /// Map a [`ChangeKind`] to a bump, applying the **pre-1.0 relaxation** based
    /// on the crate's `current` version.
    ///
    /// Post-1.0 (`current.major >= 1`): `breaking → Major`, `feature → Minor`,
    /// `fix → Patch`, `internal → Patch`.
    ///
    /// Pre-1.0 (`current.major == 0`), each level is relaxed one step:
    /// `breaking → Minor`, `feature → Patch`, `fix → Patch`, `internal → Patch`.
    pub fn from_kind(kind: ChangeKind, current: &Version) -> Self {
        if current.is_pre_1_0() {
            match kind {
                ChangeKind::Breaking => Self::Minor,
                ChangeKind::Feature | ChangeKind::Fix | ChangeKind::Internal => Self::Patch,
            }
        } else {
            match kind {
                ChangeKind::Breaking => Self::Major,
                ChangeKind::Feature => Self::Minor,
                ChangeKind::Fix | ChangeKind::Internal => Self::Patch,
            }
        }
    }

    /// The lowercase name of this bump level, for display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Major => "major",
            Self::Minor => "minor",
            Self::Patch => "patch",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    #[test]
    fn apply_bumps_the_right_component() {
        let current = version("1.2.3");
        assert_eq!(Bump::Major.apply(&current), version("2.0.0"));
        assert_eq!(Bump::Minor.apply(&current), version("1.3.0"));
        assert_eq!(Bump::Patch.apply(&current), version("1.2.4"));
    }

    #[test]
    fn apply_drops_pre_release_and_build_metadata() {
        let current = version("1.2.3-rc.1+build.9");
        // Every bump lands on a clean release version.
        assert_eq!(Bump::Patch.apply(&current), version("1.2.4"));
        assert_eq!(Bump::Minor.apply(&current), version("1.3.0"));
        assert_eq!(Bump::Major.apply(&current), version("2.0.0"));
    }

    #[test]
    fn ordering_is_major_highest() {
        assert!(Bump::Major > Bump::Minor);
        assert!(Bump::Minor > Bump::Patch);
        assert_eq!(
            [Bump::Patch, Bump::Major, Bump::Minor].iter().max(),
            Some(&Bump::Major)
        );
    }

    #[test]
    fn from_kind_post_1_0() {
        let current = version("1.2.0");
        assert_eq!(Bump::from_kind(ChangeKind::Breaking, &current), Bump::Major);
        assert_eq!(Bump::from_kind(ChangeKind::Feature, &current), Bump::Minor);
        assert_eq!(Bump::from_kind(ChangeKind::Fix, &current), Bump::Patch);
        assert_eq!(Bump::from_kind(ChangeKind::Internal, &current), Bump::Patch);
    }

    #[test]
    fn from_kind_pre_1_0_relaxes_one_level() {
        let current = version("0.3.1");
        assert_eq!(Bump::from_kind(ChangeKind::Breaking, &current), Bump::Minor);
        assert_eq!(Bump::from_kind(ChangeKind::Feature, &current), Bump::Patch);
        assert_eq!(Bump::from_kind(ChangeKind::Fix, &current), Bump::Patch);
        assert_eq!(Bump::from_kind(ChangeKind::Internal, &current), Bump::Patch);
    }
}
