//! Zaino's chain fetch and tx submission backend services.

pub mod fetch;

pub mod state;

/// Builds the gRPC [`TreeState`] shared by the Fetch and State backends from a
/// `z_gettreestate` response: hex-encoded per-pool final states (the ironwood field is
/// the empty string below NU6.3 activation, matching lightwalletd behaviour).
///
/// [`TreeState`]: zaino_proto::proto::service::TreeState
fn tree_state_from_treestate_response(
    network: String,
    treestate_response: zebra_rpc::client::GetTreestateResponse,
) -> zaino_proto::proto::service::TreeState {
    let sapling_tree = hex::encode(
        treestate_response
            .sapling()
            .commitments()
            .final_state()
            .clone()
            .unwrap_or_default(),
    );
    let orchard_tree = hex::encode(
        treestate_response
            .orchard()
            .commitments()
            .final_state()
            .clone()
            .unwrap_or_default(),
    );
    let ironwood_tree = treestate_response
        .ironwood()
        .clone()
        .and_then(|treestate| treestate.commitments().final_state().clone())
        .map(hex::encode)
        .unwrap_or_default();

    zaino_proto::proto::service::TreeState {
        network,
        height: treestate_response.height().0 as u64,
        hash: treestate_response.hash().to_string(),
        time: treestate_response.time(),
        sapling_tree,
        orchard_tree,
        ironwood_tree,
    }
}

/// Builds the `z_gettreestate` response shared by the Fetch and State backends from the
/// per-pool treestates the chain index reported.
///
/// `Commitments::new(final_root, final_state)`: the note-commitment tree is the
/// `final_state`. The ironwood treestate is `Some` only from NU6.3 activation, so
/// pre-NU6.3 responses omit the field exactly as zebrad does.
fn build_treestate_response(
    hash: zebra_chain::block::Hash,
    height: zebra_chain::block::Height,
    time: u32,
    (sapling, orchard, ironwood): (
        Option<crate::chain_index::source::PoolTreestate>,
        Option<crate::chain_index::source::PoolTreestate>,
        Option<crate::chain_index::source::PoolTreestate>,
    ),
) -> zebra_rpc::client::GetTreestateResponse {
    fn treestate(
        pool: Option<crate::chain_index::source::PoolTreestate>,
    ) -> zebra_rpc::client::Treestate {
        let (final_root, final_state) = match pool {
            Some(pool) => (pool.final_root, Some(pool.final_state)),
            None => (None, None),
        };
        zebra_rpc::client::Treestate::new(zebra_rpc::client::Commitments::new(
            final_root,
            final_state,
        ))
    }

    let sprout_treestate = None;
    let ironwood_treestate = ironwood.map(|pool| treestate(Some(pool)));
    zebra_rpc::client::GetTreestateResponse::new(
        hash,
        height,
        time,
        sprout_treestate,
        treestate(sapling),
        treestate(orchard),
        ironwood_treestate,
    )
}

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
