//! Utility functions for Zingo-RPC.

/// Zingo-Proxy build info.
pub struct BuildInfo {
    /// Git commit hash.
    pub commit_hash: String,
    /// Git Branch.
    pub branch: String,
    /// Build date.
    pub build_date: String,
    /// Build user.
    pub build_user: String,
    /// Zingo-Proxy version.
    pub version: String,
}

/// Returns build info for Zingo-Proxy.
pub fn get_build_info() -> BuildInfo {
    BuildInfo {
        commit_hash: env!("GIT_COMMIT").to_string(),
        branch: env!("BRANCH").to_string(),
        build_date: env!("BUILD_DATE").to_string(),
        build_user: env!("BUILD_USER").to_string(),
        version: env!("VERSION").to_string(),
    }
}
