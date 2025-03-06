//! Contains utility funcitonality for Zaino-State.

use zebra_chain::parameters::Network;

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

/// Returns build info for Zingo-Indexer.
pub(crate) fn get_build_info() -> BuildInfo {
    BuildInfo {
        commit_hash: env!("GIT_COMMIT").to_string(),
        branch: env!("BRANCH").to_string(),
        build_date: env!("BUILD_DATE").to_string(),
        build_user: env!("BUILD_USER").to_string(),
        version: env!("VERSION").to_string(),
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
