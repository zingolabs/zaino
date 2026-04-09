//! Service-level configuration shared across Zaino services.

/// Service-level configuration for timeouts and channels.
///
/// This configuration is used by multiple Zaino services that need to configure
/// RPC timeouts and channel buffer sizes.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ServiceConfig {
    /// Service RPC timeout in seconds
    pub timeout: u32,
    /// Service RPC maximum channel size
    pub channel_size: u32,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            timeout: 30,
            channel_size: 32,
        }
    }
}
