//! Adopting the validator's activation schedule at first contact.
//!
//! Zaino does not carry a compiled-in activation schedule for the chain it
//! serves. It reads one from the validator, verifies it, and builds the runtime
//! `Network` from it — so an indexer and its validator cannot disagree about
//! where an upgrade activates, which would silently corrupt the index.
//!
//! # Still on the old transport, deliberately
//!
//! This module reaches the validator through `zaino-fetch`'s connector rather
//! than the `zaino-source` ports, and moved here unchanged when
//! `ValidatorConnector` was deleted.
//!
//! Porting it is not mechanical. The ports report upgrades keyed by consensus
//! branch id — the protocol-defined identity — whereas this code needs zebra's
//! `NetworkUpgrade` enum to index activation slots. Elsewhere that lookup goes
//! through `network.full_activation_list()`, which is unavailable here: this
//! code is what *determines* the network, so on the regtest arm there is no
//! network to look up against yet. Resolving that belongs with the PR that owns
//! activation heights, not with the source rewire.

use indexmap::IndexMap;
use tracing::info;
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
use zebra_rpc::methods::{ConsensusBranchIdHex, NetworkUpgradeInfo};

use crate::chain_index::source::BlockchainSourceError;
use crate::config::CommonBackendConfig;

/// Constructs the runtime network at first contact with the validator.
///
/// The validator is the single source of truth for activation heights
/// (zaino#1076): the config carries only a network kind, and the runtime
/// network is constructed here before anything consumes one. On regtest the
/// schedule is adopted wholesale from the validator's report. On Mainnet and
/// The Public Testnet the compiled zebra parameters are used, but the
/// validator's reported schedule is verified against them first, so a
/// validator reporting custom activation heights under the `PubTestnet`
/// kind (a regtest net in configured-testnet clothing) fails loud here
/// instead of silently drifting from the index. There is no fallback: a
/// silently wrong schedule is the failure mode this removes.
///
/// Adoption happens exactly once, at spawn. A validator restarted
/// mid-session with a different schedule is out of scope (the test harness
/// restarts validator and indexer together); it would invalidate the index
/// without being re-detected here.
pub(crate) async fn adopt_network(
    common: &CommonBackendConfig,
    rpc_client: &JsonRpSeeConnector,
) -> Result<zebra_chain::parameters::Network, BlockchainSourceError> {
    let blockchain_info = rpc_client.get_blockchain_info().await.map_err(|error| {
        BlockchainSourceError::Unrecoverable(format!(
            "cannot fetch activation heights from the validator at {}: {error}",
            common.validator_rpc_address
        ))
    })?;

    // Shared by the Mainnet / The Public Testnet arms: the compiled network is only
    // trusted after the validator's report agrees with it.
    let verified = |network: zebra_chain::parameters::Network| {
        verify_reported_upgrades(&network, &blockchain_info.upgrades).map_err(|reason| {
            BlockchainSourceError::Unrecoverable(format!(
                "the validator at {} disagrees with the compiled {network} parameters: {reason}",
                common.validator_rpc_address
            ))
        })?;
        Ok(network)
    };

    match common.network {
        zaino_common::Network::Mainnet => verified(zebra_chain::parameters::Network::Mainnet),
        zaino_common::Network::PubTestnet => {
            verified(zebra_chain::parameters::Network::new_default_testnet())
        }
        zaino_common::Network::Regtest => {
            let heights =
                activation_heights_from_upgrades(&blockchain_info.upgrades).map_err(|reason| {
                    BlockchainSourceError::Unrecoverable(format!(
                        "cannot adopt activation heights from the validator at {}: {reason}",
                        common.validator_rpc_address
                    ))
                })?;
            info!(?heights, "Adopted activation heights from the validator");
            Ok(heights.to_regtest_network())
        }
    }
}

/// Checks every `(upgrade, height)` the validator reports against `network`'s
/// compiled activation schedule, rejecting the first disagreement.
///
/// Only reported entries are checked — the report's *completeness* is not:
/// the map is keyed by consensus branch ID, so upgrades zebra considers
/// unconfigured are simply absent, and an older validator may not know the
/// newest upgrades yet.
fn verify_reported_upgrades(
    network: &zebra_chain::parameters::Network,
    upgrades: &IndexMap<ConsensusBranchIdHex, NetworkUpgradeInfo>,
) -> Result<(), String> {
    for upgrade_info in upgrades.values() {
        let (upgrade, height, _status) = upgrade_info.into_parts();
        let compiled = upgrade.activation_height(network);
        if compiled != Some(height) {
            return Err(format!(
                "the validator reports {upgrade:?} activating at height {}, \
                 but the compiled parameters say {:?}",
                height.0,
                compiled.map(|compiled_height| compiled_height.0)
            ));
        }
    }
    Ok(())
}

/// Builds the regtest activation heights from the validator's reported
/// upgrade schedule (`getblockchaininfo.upgrades`).
///
/// The validator's configured activation heights are authoritative: the
/// config type is a payload-free kind, so both connector arms construct the
/// runtime network here at first contact, before anything consumes a
/// `Network` (zaino#1076). An upgrade absent from the validator's map is
/// never-activated — nothing is backfilled from defaults. Mainnet and
/// The Public Testnet use zebra's compiled parameters and never take this
/// path.
///
/// `before_overwinter` is always `None` here: the map is keyed by consensus
/// branch ID, which `BeforeOverwinter` does not have, so it can never appear
/// in the report. Correctness relies on zebra's `new_regtest` supplying its
/// own `BeforeOverwinter` handling when `to_regtest_network` builds the
/// runtime network from these heights.
fn activation_heights_from_upgrades(
    upgrades: &IndexMap<ConsensusBranchIdHex, NetworkUpgradeInfo>,
) -> Result<zaino_common::config::network::ActivationHeights, String> {
    let mut heights = zaino_common::config::network::ActivationHeights::NEVER_ACTIVATED;
    for upgrade_info in upgrades.values() {
        let (upgrade, height, _status) = upgrade_info.into_parts();
        // Genesis is height 0 by definition; it has no configuration slot.
        let Some(slot) = heights.slot_mut(upgrade) else {
            continue;
        };
        if slot.replace(height.0).is_some() {
            return Err(format!("validator reported {upgrade:?} twice"));
        }
    }
    Ok(heights)
}

#[cfg(test)]
mod tests {
    use zaino_common::config::network::ActivationHeights;

    /// All-`None` heights: the starting point adoption fills from the
    /// validator's map, and the expected value for every absent upgrade.
    const NEVER_ACTIVATED: ActivationHeights = ActivationHeights::NEVER_ACTIVATED;

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
}

#[cfg(test)]
mod verify_reported_upgrades {
    fn upgrades_map(
        json: &str,
    ) -> indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    > {
        serde_json::from_str(json).expect("upgrades fixture parses")
    }

    /// A report agreeing with the compiled schedule passes.
    #[test]
    fn accepts_a_report_matching_the_compiled_schedule() {
        let upgrades = upgrades_map(
            r#"{
                "76b809bb": { "name": "Sapling", "activationheight": 419200, "status": "active" }
            }"#,
        );

        super::verify_reported_upgrades(&zebra_chain::parameters::Network::Mainnet, &upgrades)
            .expect("mainnet Sapling at 419200 matches the compiled parameters");
    }

    /// A reported height disagreeing with the compiled schedule fails loud —
    /// the wrong-schedule / wrong-network drift class.
    #[test]
    fn rejects_a_mismatched_height() {
        let upgrades = upgrades_map(
            r#"{
                "76b809bb": { "name": "Sapling", "activationheight": 419201, "status": "active" }
            }"#,
        );

        let reason =
            super::verify_reported_upgrades(&zebra_chain::parameters::Network::Mainnet, &upgrades)
                .expect_err("a wrong Sapling height must be rejected");
        assert!(
            reason.contains("419201"),
            "error should name the reported height, got: {reason}"
        );
    }

    /// An upgrade the compiled schedule never activates must also be
    /// rejected when the validator reports a height for it.
    #[test]
    fn rejects_an_upgrade_the_compiled_schedule_lacks() {
        let upgrades = upgrades_map(
            r#"{
                "77190ad8": { "name": "NU7", "activationheight": 123, "status": "pending" }
            }"#,
        );

        let reason =
            super::verify_reported_upgrades(&zebra_chain::parameters::Network::Mainnet, &upgrades)
                .expect_err("an unscheduled upgrade with a reported height must be rejected");
        assert!(
            reason.contains("None"),
            "error should show the compiled schedule has no height, got: {reason}"
        );
    }
}
