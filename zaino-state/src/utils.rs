//! Contains utility funcitonality for Zaino-State.

use std::fmt;

use zaino_proto::proto::service::BlockId;
use zebra_chain::{block::Height, parameters::Network};
use zebra_state::HashOrHeight;

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

pub(crate) fn blockid_to_hashorheight(block_id: BlockId) -> Option<HashOrHeight> {
    <[u8; 32]>::try_from(block_id.hash)
        .map(zebra_chain::block::Hash)
        .map(HashOrHeight::from)
        .or_else(|_| {
            block_id
                .height
                .try_into()
                .map(|height| HashOrHeight::Height(Height(height)))
        })
        .ok()
}
