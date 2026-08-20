use serde::Deserialize;

use crate::types::{InvalidTag, Tag};

/// A short (or full) git commit sha.
///
/// Invariants, enforced once at [`parse`](Commit::parse):
/// - non-empty,
/// - every character is an ASCII hex digit.
///
/// Parse-don't-validate: once you hold a `Commit`, the string is a usable
/// abbreviated object name — short shas are fine, so no fixed length is imposed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Commit(String);

/// Why a string was rejected as a [`Commit`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCommit {
    /// The input was the empty string.
    #[error("commit sha is empty")]
    Empty,
    /// A character outside the ASCII hex-digit set appeared.
    #[error("commit sha contains a non-hex character {found:?}")]
    InvalidChar {
        /// The offending character.
        found: char,
    },
}

impl Commit {
    /// Parse a commit sha, enforcing the invariants documented on the type.
    pub fn parse(raw: &str) -> Result<Self, InvalidCommit> {
        if raw.is_empty() {
            return Err(InvalidCommit::Empty);
        }
        for ch in raw.chars() {
            if !ch.is_ascii_hexdigit() {
                return Err(InvalidCommit::InvalidChar { found: ch });
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// Borrow the validated sha.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The deployment-gate outcome for one release candidate.
///
/// The vocabulary the deployment-gate bridge reports: a run may have `passed`,
/// be `running`, have `failed`, or be `pending` (queued, not yet started).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentStatus {
    /// The deployment gate passed — the commit is eligible for `release-ready`.
    Passed,
    /// A deployment run is in progress.
    Running,
    /// The deployment gate failed (e.g. a stall).
    Failed,
    /// Queued, not yet started.
    Pending,
}

/// Why a string was rejected as a [`DeploymentStatus`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown deployment status {found:?} \
     (expected one of: passed, running, failed, pending)"
)]
pub struct InvalidDeploymentStatus {
    /// The unrecognised input.
    pub found: String,
}

impl DeploymentStatus {
    /// Parse the lowercase wire name of a deployment status. Any other value —
    /// including an empty string — is rejected, so an unknown status can never
    /// be silently treated as one of the known ones.
    pub fn parse(raw: &str) -> Result<Self, InvalidDeploymentStatus> {
        match raw {
            "passed" => Ok(Self::Passed),
            "running" => Ok(Self::Running),
            "failed" => Ok(Self::Failed),
            "pending" => Ok(Self::Pending),
            other => Err(InvalidDeploymentStatus {
                found: other.to_owned(),
            }),
        }
    }

    /// The lowercase wire word for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Running => "running",
            Self::Failed => "failed",
            Self::Pending => "pending",
        }
    }

    /// A single-glyph marker for this status, for a compact table cell.
    pub fn glyph(&self) -> &'static str {
        match self {
            Self::Passed => "✓",
            Self::Running => "●",
            Self::Failed => "✗",
            Self::Pending => "…",
        }
    }
}

/// One release candidate cut this cycle: its prerelease tag, the commit it was
/// cut from, and where its deployment run stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcEntry {
    tag: Tag,
    sha: Commit,
    deployment: DeploymentStatus,
}

impl RcEntry {
    /// Construct from already-validated parts.
    pub fn new(tag: Tag, sha: Commit, deployment: DeploymentStatus) -> Self {
        Self {
            tag,
            sha,
            deployment,
        }
    }

    /// The candidate's prerelease tag (e.g. `cycle-1-rc.1`).
    pub fn tag(&self) -> &Tag {
        &self.tag
    }

    /// The commit the candidate was cut from.
    pub fn sha(&self) -> &Commit {
        &self.sha
    }

    /// The candidate's deployment-gate status.
    pub fn deployment(&self) -> DeploymentStatus {
        self.deployment
    }
}

/// The commit at each gate branch's tip — the pipeline's live high-water marks.
///
/// Each is optional: a branch that does not yet exist (an early cycle before
/// anything reached `release-ready`, say) contributes no watermark, so the
/// renderer can omit that row rather than invent one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Watermarks {
    dev: Option<Commit>,
    rc: Option<Commit>,
    release_ready: Option<Commit>,
    stable: Option<Commit>,
}

impl Watermarks {
    /// Construct from the four (optional) branch-tip commits.
    pub fn new(
        dev: Option<Commit>,
        rc: Option<Commit>,
        release_ready: Option<Commit>,
        stable: Option<Commit>,
    ) -> Self {
        Self {
            dev,
            rc,
            release_ready,
            stable,
        }
    }

    /// The `dev` branch tip (passed the `dev`-gate).
    pub fn dev(&self) -> Option<&Commit> {
        self.dev.as_ref()
    }

    /// The `rc` branch tip (passed the `rc`-gate, under deployment).
    pub fn rc(&self) -> Option<&Commit> {
        self.rc.as_ref()
    }

    /// The `release-ready` branch tip (passed the `release`-gate).
    pub fn release_ready(&self) -> Option<&Commit> {
        self.release_ready.as_ref()
    }

    /// The `stable` branch tip (latest published release).
    pub fn stable(&self) -> Option<&Commit> {
        self.stable.as_ref()
    }
}

/// The pipeline's live git state for one release cycle, as gathered by CI and
/// handed to the renderer.
///
/// This is the pure input the release-PR dashboard renders from: the CI shell
/// gathers the git facts (branch tips, prerelease tags, deployment status) into
/// the TOML this parses, and relman stays a pure function of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleStatus {
    watermarks: Watermarks,
    rc: Vec<RcEntry>,
    released_cycle: Option<Tag>,
}

/// Everything that can go wrong parsing a [`CycleStatus`] TOML document.
///
/// Parse-don't-validate: malformed input fails here, so holders of a
/// `CycleStatus` never re-check its fields.
#[derive(Debug, thiserror::Error)]
pub enum CycleStatusError {
    /// The bytes were not valid TOML for the expected schema.
    #[error("failed to parse cycle-status TOML")]
    Toml(#[source] toml::de::Error),
    /// A `[watermarks]` value was not a valid commit sha.
    #[error("invalid watermark for {gate:?}: {value:?}")]
    InvalidWatermark {
        /// The gate the offending watermark belonged to.
        gate: &'static str,
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidCommit,
    },
    /// An `[[rc]]` entry's `tag` was not a valid tag name.
    #[error("invalid rc tag {value:?}")]
    InvalidRcTag {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidTag,
    },
    /// An `[[rc]]` entry's `sha` was not a valid commit sha.
    #[error("invalid rc sha {value:?}")]
    InvalidRcSha {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidCommit,
    },
    /// An `[[rc]]` entry's `deployment` was not a known status.
    #[error("invalid rc deployment status {value:?}")]
    InvalidDeployment {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidDeploymentStatus,
    },
    /// `released_cycle` was present but not a valid tag name.
    #[error("invalid released_cycle {value:?}")]
    InvalidReleasedCycle {
        /// The rejected raw string.
        value: String,
        /// Why it was rejected.
        #[source]
        source: InvalidTag,
    },
}

impl CycleStatus {
    /// Construct from already-validated parts.
    pub fn new(watermarks: Watermarks, rc: Vec<RcEntry>, released_cycle: Option<Tag>) -> Self {
        Self {
            watermarks,
            rc,
            released_cycle,
        }
    }

    /// Parse a cycle-status document from its TOML representation, validating
    /// every field and enforcing each newtype's invariants.
    pub fn parse_toml(input: &str) -> Result<Self, CycleStatusError> {
        let raw: RawCycleStatus = toml::from_str(input).map_err(CycleStatusError::Toml)?;
        raw.into_status()
    }

    /// The gate high-water marks.
    pub fn watermarks(&self) -> &Watermarks {
        &self.watermarks
    }

    /// The release candidates cut this cycle, in the order they were listed.
    pub fn rc(&self) -> &[RcEntry] {
        &self.rc
    }

    /// The previously-released cycle tag on `stable`, if any.
    pub fn released_cycle(&self) -> Option<&Tag> {
        self.released_cycle.as_ref()
    }
}

/// The cycle-status document, mirrored for serde.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCycleStatus {
    #[serde(default)]
    watermarks: RawWatermarks,
    #[serde(default, rename = "rc")]
    rc: Vec<RawRcEntry>,
    #[serde(default)]
    released_cycle: Option<String>,
}

/// The `[watermarks]` table; each branch tip is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWatermarks {
    dev: Option<String>,
    rc: Option<String>,
    release_ready: Option<String>,
    stable: Option<String>,
}

/// One `[[rc]]` entry, mirrored for serde.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRcEntry {
    tag: String,
    sha: String,
    deployment: String,
}

impl RawCycleStatus {
    fn into_status(self) -> Result<CycleStatus, CycleStatusError> {
        let watermarks = self.watermarks.into_watermarks()?;

        let mut rc = Vec::with_capacity(self.rc.len());
        for raw in self.rc {
            rc.push(raw.into_entry()?);
        }

        let released_cycle = match self.released_cycle {
            Some(raw) => Some(Tag::parse(&raw).map_err(|source| {
                CycleStatusError::InvalidReleasedCycle { value: raw, source }
            })?),
            None => None,
        };

        Ok(CycleStatus::new(watermarks, rc, released_cycle))
    }
}

impl RawWatermarks {
    fn into_watermarks(self) -> Result<Watermarks, CycleStatusError> {
        let parse = |gate: &'static str, raw: Option<String>| match raw {
            Some(value) => Commit::parse(&value)
                .map(Some)
                .map_err(|source| CycleStatusError::InvalidWatermark { gate, value, source }),
            None => Ok(None),
        };
        Ok(Watermarks::new(
            parse("dev", self.dev)?,
            parse("rc", self.rc)?,
            parse("release_ready", self.release_ready)?,
            parse("stable", self.stable)?,
        ))
    }
}

impl RawRcEntry {
    fn into_entry(self) -> Result<RcEntry, CycleStatusError> {
        let tag = Tag::parse(&self.tag).map_err(|source| CycleStatusError::InvalidRcTag {
            value: self.tag.clone(),
            source,
        })?;
        let sha = Commit::parse(&self.sha).map_err(|source| CycleStatusError::InvalidRcSha {
            value: self.sha.clone(),
            source,
        })?;
        let deployment = DeploymentStatus::parse(&self.deployment).map_err(|source| {
            CycleStatusError::InvalidDeployment {
                value: self.deployment.clone(),
                source,
            }
        })?;
        Ok(RcEntry::new(tag, sha, deployment))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Top-level keys (`released_cycle`) precede the `[[rc]]` array-of-tables:
    // in TOML a bare key after an array-of-tables header binds to that table,
    // so a document must place its top-level scalars first.
    const SAMPLE: &str = r#"
released_cycle = "cycle-0"

[watermarks]
dev           = "dd73705"
rc            = "dd73705"
release_ready = "dd73705"
stable        = "5e3caa1"

[[rc]]
tag        = "cycle-1-rc.1"
sha        = "dd73705"
deployment = "passed"
"#;

    fn parse_ok(input: &str) -> CycleStatus {
        CycleStatus::parse_toml(input).expect("should parse")
    }

    #[test]
    fn parses_full_document() {
        let status = parse_ok(SAMPLE);

        assert_eq!(status.watermarks().dev().expect("dev").as_str(), "dd73705");
        assert_eq!(
            status.watermarks().stable().expect("stable").as_str(),
            "5e3caa1"
        );

        assert_eq!(status.rc().len(), 1);
        let rc = &status.rc()[0];
        assert_eq!(rc.tag().as_str(), "cycle-1-rc.1");
        assert_eq!(rc.sha().as_str(), "dd73705");
        assert_eq!(rc.deployment(), DeploymentStatus::Passed);

        assert_eq!(
            status.released_cycle().expect("released").as_str(),
            "cycle-0"
        );
    }

    #[test]
    fn missing_watermarks_and_rc_and_released_cycle_are_optional() {
        // An empty document is a valid (blank) status: no watermarks, no RCs,
        // no released cycle.
        let status = parse_ok("");
        assert!(status.watermarks().dev().is_none());
        assert!(status.watermarks().rc().is_none());
        assert!(status.watermarks().release_ready().is_none());
        assert!(status.watermarks().stable().is_none());
        assert!(status.rc().is_empty());
        assert!(status.released_cycle().is_none());
    }

    #[test]
    fn omits_a_missing_watermark_branch() {
        // `release-ready` does not exist yet — its key is absent.
        let input = r#"
[watermarks]
dev = "abc1234"
stable = "def5678"
"#;
        let status = parse_ok(input);
        assert_eq!(status.watermarks().dev().expect("dev").as_str(), "abc1234");
        assert!(status.watermarks().release_ready().is_none());
        assert!(status.watermarks().rc().is_none());
    }

    #[test]
    fn parses_multiple_rc_entries_preserving_order() {
        let input = r#"
[[rc]]
tag = "cycle-2-rc.1"
sha = "aaa1111"
deployment = "failed"

[[rc]]
tag = "cycle-2-rc.2"
sha = "bbb2222"
deployment = "running"
"#;
        let status = parse_ok(input);
        assert_eq!(status.rc().len(), 2);
        assert_eq!(status.rc()[0].tag().as_str(), "cycle-2-rc.1");
        assert_eq!(status.rc()[0].deployment(), DeploymentStatus::Failed);
        assert_eq!(status.rc()[1].tag().as_str(), "cycle-2-rc.2");
        assert_eq!(status.rc()[1].deployment(), DeploymentStatus::Running);
    }

    #[test]
    fn rejects_unknown_deployment_status() {
        let input = r#"
[[rc]]
tag = "cycle-1-rc.1"
sha = "dd73705"
deployment = "green"
"#;
        assert!(matches!(
            CycleStatus::parse_toml(input),
            Err(CycleStatusError::InvalidDeployment { value, .. }) if value == "green"
        ));
    }

    #[test]
    fn rejects_non_hex_watermark() {
        let input = r#"
[watermarks]
dev = "zzzzzzz"
"#;
        assert!(matches!(
            CycleStatus::parse_toml(input),
            Err(CycleStatusError::InvalidWatermark { gate: "dev", .. })
        ));
    }

    #[test]
    fn rejects_bad_rc_tag() {
        let input = r#"
[[rc]]
tag = "has space"
sha = "dd73705"
deployment = "pending"
"#;
        assert!(matches!(
            CycleStatus::parse_toml(input),
            Err(CycleStatusError::InvalidRcTag { .. })
        ));
    }

    #[test]
    fn deployment_status_parse_round_trips_and_has_glyphs() {
        for status in [
            DeploymentStatus::Passed,
            DeploymentStatus::Running,
            DeploymentStatus::Failed,
            DeploymentStatus::Pending,
        ] {
            assert_eq!(DeploymentStatus::parse(status.as_str()), Ok(status));
            assert!(!status.glyph().is_empty());
        }
        assert_eq!(DeploymentStatus::Passed.glyph(), "✓");
        assert_eq!(DeploymentStatus::Running.glyph(), "●");
        assert_eq!(DeploymentStatus::Failed.glyph(), "✗");
        assert_eq!(DeploymentStatus::Pending.glyph(), "…");
    }

    #[test]
    fn commit_rejects_empty_and_non_hex() {
        assert_eq!(Commit::parse(""), Err(InvalidCommit::Empty));
        assert_eq!(
            Commit::parse("dead-beef"),
            Err(InvalidCommit::InvalidChar { found: '-' })
        );
        assert_eq!(Commit::parse("dd73705").expect("hex").as_str(), "dd73705");
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        // `deny_unknown_fields` guards against a typo'd section silently
        // contributing nothing.
        assert!(matches!(
            CycleStatus::parse_toml("watermrks = {}\n"),
            Err(CycleStatusError::Toml(_))
        ));
    }
}
