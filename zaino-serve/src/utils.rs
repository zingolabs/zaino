//! Utility functions for Zingo-RPC.

/// Zingo-Indexer build info.
pub(crate) struct BuildInfo {
    /// Git commit hash.
    pub commit_hash: String,
    /// Git Branch.
    pub branch: String,
    /// Build date.
    pub build_date: String,
    /// Build user.
    pub build_user: String,
    /// Zingo-Indexer version.
    pub version: String,
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
