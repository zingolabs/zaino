//! Contains utility funcitonality for Zaino-State.
use std::fmt;
use zebra_chain::parameters::Network;

// *** Metadata structs ***

/// Zaino build info.
#[derive(Debug, Clone)]
pub(crate) struct BuildInfo {
    /// Git commit hash.
    commit_hash: String,
    /// Git Branch.
    branch: String,
    /// Build date.
    build_date: String,
    /// Build user.
    build_user: String,
    /// Zingo-Indexer version.
    version: String,
}

#[allow(dead_code)]
impl BuildInfo {
    pub(crate) fn commit_hash(&self) -> String {
        self.commit_hash.clone()
    }

    pub(crate) fn branch(&self) -> String {
        self.branch.clone()
    }

    pub(crate) fn build_user(&self) -> String {
        self.build_user.clone()
    }

    pub(crate) fn build_date(&self) -> String {
        self.build_date.clone()
    }

    pub(crate) fn version(&self) -> String {
        self.version.clone()
    }
}

impl fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Version: {}", self.version)?;
        writeln!(f, "Commit Hash: {}", self.commit_hash)?;
        writeln!(f, "Branch: {}", self.branch)?;
        writeln!(f, "Build Date: {}", self.build_date)?;
        writeln!(f, "Build User: {}", self.build_user)
    }
}

/// Returns build info for Zingo-Indexer.
///
/// `version` is the version of the deployed indexer binary (e.g. `zainod`),
/// supplied by the caller. Library crates do not know which binary embeds
/// them, so each binary passes its own `CARGO_PKG_VERSION`.
pub(crate) fn get_build_info(version: String) -> BuildInfo {
    BuildInfo {
        commit_hash: env!("GIT_COMMIT").to_string(),
        branch: env!("BRANCH").to_string(),
        build_date: env!("BUILD_DATE").to_string(),
        build_user: env!("BUILD_USER").to_string(),
        version,
    }
}

#[derive(Debug, Clone)]
pub struct ServiceMetadata {
    build_info: BuildInfo,
    network: Network,
    zebra_build: String,
    zebra_subversion: String,
}

impl ServiceMetadata {
    pub(crate) fn new(
        build_info: BuildInfo,
        network: Network,
        zebra_build: String,
        zebra_subversion: String,
    ) -> Self {
        Self {
            build_info,
            network,
            zebra_build,
            zebra_subversion,
        }
    }

    pub(crate) fn build_info(&self) -> BuildInfo {
        self.build_info.clone()
    }

    pub(crate) fn network(&self) -> Network {
        self.network.clone()
    }

    pub(crate) fn zebra_build(&self) -> String {
        self.zebra_build.clone()
    }

    pub(crate) fn zebra_subversion(&self) -> String {
        self.zebra_subversion.clone()
    }
}

impl fmt::Display for ServiceMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Zaino Service Metadata")?;
        writeln!(f, "-----------------------")?;
        writeln!(f, "Build Info:\n{}", self.build_info)?;
        writeln!(f, "Network: {}", self.network)?;
        writeln!(f, "Zebra Build: {}", self.zebra_build)?;
        writeln!(f, "Zebra Subversion: {}", self.zebra_subversion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for issue #1057: the version flowed onto the wire via
    /// `LightdInfo.version` must come from the caller-supplied string (the
    /// embedding binary's `CARGO_PKG_VERSION`), not from this library crate.
    #[test]
    fn get_build_info_uses_caller_supplied_version() {
        let build_info = get_build_info("9.9.9-test".to_string());
        assert_eq!(build_info.version(), "9.9.9-test");
    }
}
