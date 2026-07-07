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

/// Builds the regtest activation heights from the validator's reported
/// upgrade schedule (`getblockchaininfo.upgrades`).
///
/// The validator's configured activation heights are authoritative: the
/// config type is a payload-free kind, so both backends construct the
/// runtime network here at first contact, before anything consumes a
/// `Network` (zaino#1076). An upgrade absent from the validator's map is
/// never-activated — nothing is backfilled from defaults. Mainnet and
/// Testnet use zebra's compiled parameters and never take this path.
fn activation_heights_from_upgrades(
    upgrades: &indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    >,
) -> Result<zaino_common::config::network::ActivationHeights, String> {
    use zebra_chain::parameters::NetworkUpgrade;

    let mut heights = zaino_common::config::network::ActivationHeights {
        before_overwinter: None,
        overwinter: None,
        sapling: None,
        blossom: None,
        heartwood: None,
        canopy: None,
        nu5: None,
        nu6: None,
        nu6_1: None,
        nu6_2: None,
        nu6_3: None,
        nu7: None,
    };
    for upgrade_info in upgrades.values() {
        let (upgrade, height, _status) = upgrade_info.into_parts();
        let slot = match upgrade {
            // Genesis is height 0 by definition; it has no configuration slot.
            NetworkUpgrade::Genesis => continue,
            NetworkUpgrade::BeforeOverwinter => &mut heights.before_overwinter,
            NetworkUpgrade::Overwinter => &mut heights.overwinter,
            NetworkUpgrade::Sapling => &mut heights.sapling,
            NetworkUpgrade::Blossom => &mut heights.blossom,
            NetworkUpgrade::Heartwood => &mut heights.heartwood,
            NetworkUpgrade::Canopy => &mut heights.canopy,
            NetworkUpgrade::Nu5 => &mut heights.nu5,
            NetworkUpgrade::Nu6 => &mut heights.nu6,
            NetworkUpgrade::Nu6_1 => &mut heights.nu6_1,
            NetworkUpgrade::Nu6_2 => &mut heights.nu6_2,
            NetworkUpgrade::Nu6_3 => &mut heights.nu6_3,
            NetworkUpgrade::Nu7 => &mut heights.nu7,
        };
        if slot.replace(height.0).is_some() {
            return Err(format!("validator reported {upgrade:?} twice"));
        }
    }
    Ok(heights)
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
    use zaino_common::config::network::ActivationHeights;

    /// All-`None` heights: the starting point adoption fills from the
    /// validator's map, and the expected value for every absent upgrade.
    const NEVER_ACTIVATED: ActivationHeights = ActivationHeights {
        before_overwinter: None,
        overwinter: None,
        sapling: None,
        blossom: None,
        heartwood: None,
        canopy: None,
        nu5: None,
        nu6: None,
        nu6_1: None,
        nu6_2: None,
        nu6_3: None,
        nu7: None,
    };

    fn upgrades_map(
        json: &str,
    ) -> indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    > {
        serde_json::from_str(json).expect("upgrades fixture parses")
    }

    fn adopted_heights(
        upgrades: &indexmap::IndexMap<
            zebra_rpc::methods::ConsensusBranchIdHex,
            zebra_rpc::methods::NetworkUpgradeInfo,
        >,
    ) -> ActivationHeights {
        super::activation_heights_from_upgrades(upgrades).expect("valid schedule")
    }

    /// An upgrade absent from the validator's map is never-activated —
    /// nothing is backfilled from any default schedule.
    #[test]
    fn regtest_network_from_upgrades_leaves_absent_upgrades_never_activated() {
        let upgrades = upgrades_map(
            r#"{
                "c2d6d0b4": { "name": "NU5", "activationheight": 2, "status": "active" },
                "c8e71055": { "name": "NU6", "activationheight": 2, "status": "active" }
            }"#,
        );

        assert_eq!(
            adopted_heights(&upgrades),
            ActivationHeights {
                nu5: Some(2),
                nu6: Some(2),
                ..NEVER_ACTIVATED
            }
        );
    }

    /// The ORCHARD_THEN_IRONWOOD transition shape: everything through NU6.2
    /// at 1–2, NU6.3 at 6 — the schedule the ironwood_activation fixtures
    /// launch validators with.
    #[test]
    fn regtest_network_from_upgrades_reads_a_transition_schedule() {
        let upgrades = upgrades_map(
            r#"{
                "5ba81b19": { "name": "Overwinter", "activationheight": 1, "status": "active" },
                "76b809bb": { "name": "Sapling", "activationheight": 1, "status": "active" },
                "2bb40e60": { "name": "Blossom", "activationheight": 1, "status": "active" },
                "f5b9230b": { "name": "Heartwood", "activationheight": 1, "status": "active" },
                "e9ff75a6": { "name": "Canopy", "activationheight": 1, "status": "active" },
                "c2d6d0b4": { "name": "NU5", "activationheight": 2, "status": "active" },
                "c8e71055": { "name": "NU6", "activationheight": 2, "status": "active" },
                "4dec4df0": { "name": "NU6.1", "activationheight": 2, "status": "active" },
                "5437f330": { "name": "NU6.2", "activationheight": 2, "status": "active" },
                "37a5165b": { "name": "NU6.3", "activationheight": 6, "status": "pending" }
            }"#,
        );

        assert_eq!(
            adopted_heights(&upgrades),
            ActivationHeights {
                overwinter: Some(1),
                sapling: Some(1),
                blossom: Some(1),
                heartwood: Some(1),
                canopy: Some(1),
                nu5: Some(2),
                nu6: Some(2),
                nu6_1: Some(2),
                nu6_2: Some(2),
                nu6_3: Some(6),
                ..NEVER_ACTIVATED
            }
        );
    }

    /// A validator reporting the same upgrade twice is nonsense; adoption
    /// must fail loudly rather than pick a height.
    #[test]
    fn regtest_network_from_upgrades_rejects_a_duplicate_upgrade() {
        let upgrades = upgrades_map(
            r#"{
                "c2d6d0b4": { "name": "NU5", "activationheight": 2, "status": "active" },
                "c8e71055": { "name": "NU5", "activationheight": 3, "status": "pending" }
            }"#,
        );

        let reason =
            super::activation_heights_from_upgrades(&upgrades).expect_err("duplicate must fail");
        assert!(
            reason.contains("twice"),
            "error should name the duplication, got: {reason}"
        );
    }

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
