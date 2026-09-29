//! Open-file limit vs `grpc.max_connections` (one fd per connection; past the limit `accept()`
//! fails with `EMFILE`, and an index open would too)

use std::num::NonZeroUsize;

use tracing::info;

use crate::error::IndexerError;

/// fds besides client sockets: index files (~290 measured on a syncing mainnet node, bounded by
/// tree levels + LSM segment caps), validator RPC pools, metrics, logs
const RESERVED: u64 = 1024;

/// Raises the soft limit to the hard one, then refuses a `max_connections` it cannot hold
pub(crate) fn raise_for(max_connections: NonZeroUsize) -> Result<(), IndexerError> {
    let limit = rlimit::increase_nofile_limit(u64::MAX).map_err(|error| {
        IndexerError::ConfigError(format!("raising the open-file limit: {error}"))
    })?;
    fits(max_connections, limit)?;
    info!(limit, "Open-file limit");
    Ok(())
}

fn fits(max_connections: NonZeroUsize, limit: u64) -> Result<(), IndexerError> {
    let needed = u64::try_from(max_connections.get()).unwrap_or(u64::MAX).saturating_add(RESERVED);
    match needed <= limit {
        true => Ok(()),
        false => Err(IndexerError::ConfigError(format!(
            "grpc.max_connections = {max_connections} needs {needed} open files ({RESERVED} kept \
             for indexes and the validator) and the limit is {limit}: raise the hard limit \
             (systemd LimitNOFILE=, docker --ulimit nofile=) or lower grpc.max_connections"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connections + the reserve must fit; the refusal names the knob and both remedies
    #[test]
    fn max_connections_plus_the_reserve_must_fit_the_limit() {
        let connections = |n| NonZeroUsize::new(n).expect("non-zero");

        assert!(fits(connections(4096), 4096 + RESERVED).is_ok(), "exactly fits");
        let refused = fits(connections(4096), 1024).expect_err("systemd's default soft limit");
        let message = refused.to_string();
        for named in ["grpc.max_connections = 4096", "needs 5120", "limit is 1024", "LimitNOFILE"] {
            assert!(message.contains(named), "{named}: {message}");
        }
    }
}
