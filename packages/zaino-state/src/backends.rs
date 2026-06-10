//! Zaino's chain fetch and tx submission backend services.

pub mod fetch;

pub mod state;

fn latest_network_upgrade(
    upgrades: &indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    >,
) -> Result<&zebra_rpc::methods::NetworkUpgradeInfo, tonic::Status> {
    upgrades.last().map(|(_, upgrade)| upgrade).ok_or_else(|| {
        tonic::Status::failed_precondition("validator returned no network upgrade metadata")
    })
}

/// Maximum number of addresses a single `get_address_utxos` / `get_address_utxos_stream`
/// request may carry.
///
/// Both backends resolve the full backend UTXO set before applying `max_entries` /
/// `start_height` (issue #974). A complete pushdown fix needs upstream interface changes
/// the caller-supplied entry cap cannot reach today, so until then this bounds the one
/// input the service controls locally: the address fan-out. It stops an unauthenticated
/// caller forcing an unbounded number of backend address lookups in a single request, and
/// is set well above realistic wallet usage.
///
/// TODO: make this deployment-configurable rather than a fixed constant.
const UTXO_MAX_ADDRESSES: usize = 1000;

/// Reject a `get_address_utxos` request whose address list exceeds [`UTXO_MAX_ADDRESSES`].
///
/// `max_entries` bounds the response size, not the backend work; this guard bounds the
/// address fan-out, the part the service can cap without upstream changes.
fn validate_utxo_address_count(count: usize) -> Result<(), tonic::Status> {
    if count > UTXO_MAX_ADDRESSES {
        return Err(tonic::Status::invalid_argument(format!(
            "Error: too many addresses in request: {count} exceeds the maximum of {UTXO_MAX_ADDRESSES}."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn latest_network_upgrade_rejects_empty_metadata() {
        let upgrades = indexmap::IndexMap::new();
        let err = super::latest_network_upgrade(&upgrades).expect_err("empty upgrades must fail");

        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            err.message(),
            "validator returned no network upgrade metadata"
        );
    }

    #[test]
    fn utxo_address_count_within_limit_is_accepted() {
        assert!(super::validate_utxo_address_count(0).is_ok());
        assert!(super::validate_utxo_address_count(super::UTXO_MAX_ADDRESSES).is_ok());
    }

    #[test]
    fn utxo_address_count_over_limit_is_rejected() {
        let err = super::validate_utxo_address_count(super::UTXO_MAX_ADDRESSES + 1)
            .expect_err("over-limit address count must fail");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
