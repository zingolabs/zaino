//! Network utilities for Zaino.

use std::net::{SocketAddr, ToSocketAddrs};

/// Result of attempting to resolve an address string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressResolution {
    /// Successfully resolved to a socket address.
    Resolved(SocketAddr),
    /// Address appears to be a valid hostname:port format but DNS lookup failed.
    /// This is acceptable for deferred resolution (e.g., Docker DNS).
    UnresolvedHostname {
        /// The original address string.
        address: String,
        /// The DNS error message.
        error: String,
    },
    /// Address format is invalid (missing port, garbage input, etc.).
    /// This should always be treated as an error.
    InvalidFormat {
        /// The original address string.
        address: String,
        /// Description of what's wrong with the format.
        reason: String,
    },
}

impl AddressResolution {
    /// Returns the resolved address if available.
    pub fn resolved(&self) -> Option<SocketAddr> {
        match self {
            AddressResolution::Resolved(addr) => Some(*addr),
            _ => None,
        }
    }

    /// Returns true if the address was successfully resolved.
    pub fn is_resolved(&self) -> bool {
        matches!(self, AddressResolution::Resolved(_))
    }

    /// Returns true if the address has a valid format but couldn't be resolved.
    /// This is acceptable for deferred resolution scenarios like Docker DNS.
    pub fn is_unresolved_hostname(&self) -> bool {
        matches!(self, AddressResolution::UnresolvedHostname { .. })
    }

    /// Returns true if the address format is invalid.
    pub fn is_invalid_format(&self) -> bool {
        matches!(self, AddressResolution::InvalidFormat { .. })
    }
}

/// Validates that an address string has a valid format (host:port).
///
/// This performs basic format validation without DNS lookup:
/// - Must contain exactly one `:` separator (or be IPv6 format `[...]:port`)
/// - Port must be a valid number
/// - Host part must not be empty
fn validate_address_format(address: &str) -> Result<(), String> {
    let address = address.trim();

    if address.is_empty() {
        return Err("Address cannot be empty".to_string());
    }

    // Handle IPv6 format: [::1]:port
    if address.starts_with('[') {
        let Some(bracket_end) = address.find(']') else {
            return Err("IPv6 address missing closing bracket".to_string());
        };

        if bracket_end + 1 >= address.len() {
            return Err("Missing port after IPv6 address".to_string());
        }

        let after_bracket = &address[bracket_end + 1..];
        if !after_bracket.starts_with(':') {
            return Err("Expected ':' after IPv6 address bracket".to_string());
        }

        let port_str = &after_bracket[1..];
        port_str
            .parse::<u16>()
            .map_err(|_| format!("Invalid port number: '{port_str}'"))?;

        return Ok(());
    }

    // Handle IPv4/hostname format: host:port
    let parts: Vec<&str> = address.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        return Err("Missing port (expected format: 'host:port')".to_string());
    }

    let port_str = parts[0];
    let host = parts[1];

    if host.is_empty() {
        return Err("Host cannot be empty".to_string());
    }

    port_str
        .parse::<u16>()
        .map_err(|_| format!("Invalid port number: '{port_str}'"))?;

    Ok(())
}

/// Attempts to resolve an address string, returning detailed information about the result.
///
/// This function distinguishes between:
/// - Successfully resolved addresses
/// - Valid hostname:port format that failed DNS lookup (acceptable for Docker DNS)
/// - Invalid address format (always an error)
///
/// # Examples
///
/// ```
/// use zaino_common::net::{try_resolve_address, AddressResolution};
///
/// // IP:port format resolves immediately
/// let result = try_resolve_address("127.0.0.1:8080");
/// assert!(result.is_resolved());
///
/// // Invalid format is detected
/// let result = try_resolve_address("no-port-here");
/// assert!(result.is_invalid_format());
/// ```
pub fn try_resolve_address(address: &str) -> AddressResolution {
    // First validate the format
    if let Err(reason) = validate_address_format(address) {
        return AddressResolution::InvalidFormat {
            address: address.to_string(),
            reason,
        };
    }

    // Try parsing as SocketAddr first (handles ip:port format directly)
    if let Ok(addr) = address.parse::<SocketAddr>() {
        return AddressResolution::Resolved(addr);
    }

    // Fall back to DNS resolution for hostname:port format
    match address.to_socket_addrs() {
        Ok(mut addrs) => {
            let addrs_vec: Vec<SocketAddr> = addrs.by_ref().collect();

            // Prefer IPv4 if available (more compatible, especially in Docker)
            if let Some(ipv4_addr) = addrs_vec.iter().find(|addr| addr.is_ipv4()) {
                AddressResolution::Resolved(*ipv4_addr)
            } else if let Some(addr) = addrs_vec.into_iter().next() {
                AddressResolution::Resolved(addr)
            } else {
                AddressResolution::UnresolvedHostname {
                    address: address.to_string(),
                    error: "DNS returned no addresses".to_string(),
                }
            }
        }
        Err(e) => AddressResolution::UnresolvedHostname {
            address: address.to_string(),
            error: e.to_string(),
        },
    }
}

/// Resolves an address string to a [`SocketAddr`].
///
/// Accepts both IP:port format (e.g., "127.0.0.1:8080") and hostname:port format
/// (e.g., "zebra:18232" for Docker DNS resolution).
///
/// When both IPv4 and IPv6 addresses are available, IPv4 is preferred.
///
/// # Examples
///
/// ```
/// use zaino_common::net::resolve_socket_addr;
///
/// // IP:port format
/// let addr = resolve_socket_addr("127.0.0.1:8080").unwrap();
/// assert_eq!(addr.port(), 8080);
///
/// // Hostname resolution (localhost)
/// let addr = resolve_socket_addr("localhost:8080").unwrap();
/// assert!(addr.ip().is_loopback());
/// ```
///
/// # Errors
///
/// Returns an error if:
/// - The address format is invalid (missing port, invalid IP, etc.)
/// - The hostname cannot be resolved (DNS lookup failure)
/// - No addresses are returned from resolution
pub fn resolve_socket_addr(address: &str) -> Result<SocketAddr, std::io::Error> {
    match try_resolve_address(address) {
        AddressResolution::Resolved(addr) => Ok(addr),
        AddressResolution::UnresolvedHostname { address, error } => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Cannot resolve hostname '{address}': {error}"),
        )),
        AddressResolution::InvalidFormat { address, reason } => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Invalid address format '{address}': {reason}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // === Format validation tests (no DNS, always reliable) ===

    #[test]
    fn test_resolve_ipv4_address() {
        let result = resolve_socket_addr("127.0.0.1:8080");
        assert!(result.is_ok());
        let addr = result.unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 8080);
    }

    #[test]
    fn test_resolve_ipv4_any_address() {
        let result = resolve_socket_addr("0.0.0.0:18232");
        assert!(result.is_ok());
        let addr = result.unwrap();
        assert_eq!(addr.ip(), Ipv4Addr::UNSPECIFIED);
        assert_eq!(addr.port(), 18232);
    }

    #[test]
    fn test_resolve_ipv6_localhost() {
        let result = resolve_socket_addr("[::1]:8080");
        assert!(result.is_ok());
        let addr = result.unwrap();
        assert!(addr.is_ipv6());
        assert_eq!(addr.port(), 8080);
    }

    #[test]
    fn test_resolve_missing_port() {
        let result = try_resolve_address("127.0.0.1");
        assert!(result.is_invalid_format());
    }

    #[test]
    fn test_resolve_empty_string() {
        let result = try_resolve_address("");
        assert!(result.is_invalid_format());
    }

    #[test]
    fn test_resolve_invalid_port() {
        let result = try_resolve_address("127.0.0.1:invalid");
        assert!(result.is_invalid_format());
    }

    #[test]
    fn test_resolve_port_too_large() {
        let result = try_resolve_address("127.0.0.1:99999");
        assert!(result.is_invalid_format());
    }

    #[test]
    fn test_resolve_empty_host() {
        let result = try_resolve_address(":8080");
        assert!(result.is_invalid_format());
    }

    #[test]
    fn test_resolve_ipv6_missing_port() {
        let result = try_resolve_address("[::1]");
        assert!(result.is_invalid_format());
    }

    #[test]
    fn test_resolve_ipv6_missing_bracket() {
        let result = try_resolve_address("[::1:8080");
        assert!(result.is_invalid_format());
    }

    #[test]
    fn test_valid_hostname_format() {
        // This hostname has valid format but won't resolve
        let result = try_resolve_address("nonexistent-host.invalid:8080");
        // Should be unresolved hostname, not invalid format
        assert!(
            result.is_unresolved_hostname(),
            "Expected UnresolvedHostname, got {:?}",
            result
        );
    }

    #[test]
    fn test_docker_style_hostname_format() {
        // Docker-style hostnames have valid format
        let result = try_resolve_address("zebra:18232");
        // Can't resolve in unit tests, but format is valid
        assert!(
            result.is_unresolved_hostname(),
            "Expected UnresolvedHostname for Docker-style hostname, got {:?}",
            result
        );
    }

    // === DNS-dependent tests (may be flaky in CI) ===

    #[test]
    #[ignore = "DNS-dependent: may be flaky in CI environments without reliable DNS"]
    fn test_resolve_hostname_localhost() {
        // "localhost" should resolve to 127.0.0.1 or ::1
        let result = resolve_socket_addr("localhost:8080");
        assert!(result.is_ok());
        let addr = result.unwrap();
        assert_eq!(addr.port(), 8080);
        assert!(addr.ip().is_loopback());
    }

    #[test]
    #[ignore = "DNS-dependent: behavior varies by system DNS configuration"]
    fn test_resolve_invalid_hostname_dns() {
        // This test verifies DNS lookup failure for truly invalid hostnames
        let result = resolve_socket_addr("this-hostname-does-not-exist.invalid:8080");
        assert!(result.is_err());
    }
}
