//! Adopting the validator's activation schedule at first contact.
//!
//! Zaino does not carry a compiled-in activation schedule for the chain it
//! serves. It reads one from the validator, verifies it, and builds the runtime
//! `Network` from it — so an indexer and its validator cannot disagree about
//! where an upgrade activates, which would silently corrupt the index.
//!
//! # On the source ports
//!
//! This module reads `getblockchaininfo` through the `zaino-source`
//! [`GetBlockchainInfo`] port, so it never deserializes the validator's own
//! response shape — in particular it never parses the value pools, whose ZEC
//! `chainValue` float zebra's own type rejects when it does not round-trip to a
//! whole zatoshi (the mainnet-boot crash this replaced).
//!
//! The port yields the domain upgrade schedule keyed by consensus branch id —
//! the protocol-defined identity. Each branch id is translated to zebra's
//! `NetworkUpgrade` locally, only where a zebra `Network` must be built. A
//! branch id this build does not recognise is a validator ahead of Zaino: it is
//! skipped, never rejected, so a newer validator cannot fail adoption.

use tracing::info;
use zaino_primitives::types::NetworkUpgradeInfo;
use zaino_source::GetBlockchainInfo;

use crate::chain_index::source::BlockchainSourceError;
use crate::config::CommonBackendConfig;

mod zebra_bridge;

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
    source: &impl GetBlockchainInfo,
) -> Result<zebra_chain::parameters::Network, BlockchainSourceError> {
    // Read the schedule through the source port: the domain `BlockchainInfo`,
    // not the validator's own response shape. This never parses the value pools
    // (whose ZEC float zebra's type rejects when it does not round-trip to a
    // whole zatoshi — the mainnet-boot crash this replaced); only the upgrade
    // schedule below is used.
    let blockchain_info = source.get_blockchain_info().await.map_err(|error| {
        BlockchainSourceError::Unrecoverable(format!(
            "cannot fetch activation heights from the validator at {}: {error}",
            common.validator_rpc_address
        ))
    })?;
    let upgrades = &blockchain_info.upgrades;

    // Shared by the Mainnet / The Public Testnet arms: the compiled network is only
    // trusted after the validator's report agrees with it.
    let verified = |network: zebra_chain::parameters::Network| {
        verify_reported_upgrades(&network, upgrades).map_err(|reason| {
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
            let heights = activation_heights_from_upgrades(upgrades).map_err(|reason| {
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
/// Only reported entries are checked — the report's *completeness* is not, and
/// an upgrade whose branch id this build does not know is skipped (see
/// [`zebra_bridge::network_upgrade`]), so a validator ahead of Zaino is never
/// rejected here.
fn verify_reported_upgrades(
    network: &zebra_chain::parameters::Network,
    upgrades: &[NetworkUpgradeInfo],
) -> Result<(), String> {
    for upgrade_info in upgrades {
        let Some(upgrade) = zebra_bridge::network_upgrade(upgrade_info) else {
            continue;
        };
        let reported = zebra_bridge::height(upgrade_info.activation_height);
        let compiled = upgrade.activation_height(network);
        if compiled != Some(reported) {
            return Err(format!(
                "the validator reports {upgrade:?} activating at height {}, \
                 but the compiled parameters say {:?}",
                reported.0,
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
/// `before_overwinter` is always `None` here: entries are keyed by consensus
/// branch ID, which `BeforeOverwinter` does not have, so it can never appear
/// in the report. Correctness relies on zebra's `new_regtest` supplying its
/// own `BeforeOverwinter` handling when `to_regtest_network` builds the
/// runtime network from these heights.
fn activation_heights_from_upgrades(
    upgrades: &[NetworkUpgradeInfo],
) -> Result<zaino_common::config::network::ActivationHeights, String> {
    let mut heights = zaino_common::config::network::ActivationHeights::NEVER_ACTIVATED;
    for upgrade_info in upgrades {
        // A branch id this build does not know has no configuration slot.
        let Some(upgrade) = zebra_bridge::network_upgrade(upgrade_info) else {
            continue;
        };
        // Genesis is height 0 by definition; it has no configuration slot.
        let Some(slot) = heights.slot_mut(upgrade) else {
            continue;
        };
        if slot
            .replace(u32::from(upgrade_info.activation_height))
            .is_some()
        {
            return Err(format!("validator reported {upgrade:?} twice"));
        }
    }
    Ok(heights)
}

/// Shared fixtures for the adoption tests: the consensus branch ids the code
/// keys on, named so tests read as schedules rather than hex, plus the builder.
#[cfg(test)]
mod test_upgrades {
    use zaino_primitives::types::{
        ConsensusBranchId, Height, NetworkUpgradeInfo, NetworkUpgradeStatus,
    };

    pub(super) const OVERWINTER: u32 = 0x5ba8_1b19;
    pub(super) const SAPLING: u32 = 0x76b8_09bb;
    pub(super) const BLOSSOM: u32 = 0x2bb4_0e60;
    pub(super) const HEARTWOOD: u32 = 0xf5b9_230b;
    pub(super) const CANOPY: u32 = 0xe9ff_75a6;
    pub(super) const NU5: u32 = 0xc2d6_d0b4;
    pub(super) const NU6: u32 = 0xc8e7_1055;
    pub(super) const NU6_1: u32 = 0x4dec_4df0;
    pub(super) const NU6_2: u32 = 0x5437_f330;
    pub(super) const NU6_3: u32 = 0x37a5_165b;

    /// zebra's placeholder branch for NU7 (the real `0x77190ad8` is still a TODO
    /// in zebra-chain): resolvable, and disabled on Mainnet.
    pub(super) const NU7: u32 = 0xffff_fffe;

    /// A branch id no build knows.
    pub(super) const UNKNOWN_BRANCH: u32 = 0x0bad_0bad;

    /// Sapling's compiled Mainnet activation height.
    pub(super) const SAPLING_MAINNET_HEIGHT: u32 = 419_200;

    /// An upgrade entry. Only the branch id and height matter to the code under
    /// test, so the name and status are fixed.
    pub(super) fn upgrade(branch_id: u32, activation_height: u32) -> NetworkUpgradeInfo {
        NetworkUpgradeInfo {
            branch_id: ConsensusBranchId::new(branch_id),
            name: String::new(),
            activation_height: Height::try_from(activation_height).expect("height within range"),
            status: NetworkUpgradeStatus::Active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_upgrades::*;
    use zaino_common::config::network::ActivationHeights;
    use zaino_primitives::types::NetworkUpgradeInfo;

    /// All-`None` heights: the starting point adoption fills from the
    /// validator's report, and the expected value for every absent upgrade.
    const NEVER_ACTIVATED: ActivationHeights = ActivationHeights::NEVER_ACTIVATED;

    fn adopted_heights(upgrades: &[NetworkUpgradeInfo]) -> ActivationHeights {
        super::activation_heights_from_upgrades(upgrades).expect("valid schedule")
    }

    /// An upgrade absent from the validator's report is never-activated —
    /// nothing is backfilled from any default schedule.
    #[test]
    fn regtest_network_from_upgrades_leaves_absent_upgrades_never_activated() {
        let upgrades = [upgrade(NU5, 2), upgrade(NU6, 2)];

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
        let upgrades = [
            upgrade(OVERWINTER, 1),
            upgrade(SAPLING, 1),
            upgrade(BLOSSOM, 1),
            upgrade(HEARTWOOD, 1),
            upgrade(CANOPY, 1),
            upgrade(NU5, 2),
            upgrade(NU6, 2),
            upgrade(NU6_1, 2),
            upgrade(NU6_2, 2),
            upgrade(NU6_3, 6),
        ];

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

    /// A validator reporting the same upgrade twice is nonsense; adoption must
    /// fail loudly rather than pick a height. Entries are keyed by branch id,
    /// so a duplicate is the *same* branch id, not merely a repeated name.
    #[test]
    fn regtest_network_from_upgrades_rejects_a_duplicate_upgrade() {
        let upgrades = [upgrade(NU5, 2), upgrade(NU5, 3)];

        let reason =
            super::activation_heights_from_upgrades(&upgrades).expect_err("duplicate must fail");
        assert!(
            reason.contains("twice"),
            "error should name the duplication, got: {reason}"
        );
    }

    /// An upgrade whose branch id this build does not recognise is a validator
    /// ahead of Zaino: skipped, neither adopted nor an error.
    #[test]
    fn regtest_network_from_upgrades_skips_an_unknown_branch() {
        let upgrades = [upgrade(NU5, 2), upgrade(UNKNOWN_BRANCH, 9)];

        assert_eq!(
            adopted_heights(&upgrades),
            ActivationHeights {
                nu5: Some(2),
                ..NEVER_ACTIVATED
            }
        );
    }
}

#[cfg(test)]
mod verify_reported_upgrades {
    use super::test_upgrades::*;

    /// A report agreeing with the compiled schedule passes.
    #[test]
    fn accepts_a_report_matching_the_compiled_schedule() {
        let upgrades = [upgrade(SAPLING, SAPLING_MAINNET_HEIGHT)];

        super::verify_reported_upgrades(&zebra_chain::parameters::Network::Mainnet, &upgrades)
            .expect("mainnet Sapling at its compiled height matches");
    }

    /// A reported height disagreeing with the compiled schedule fails loud —
    /// the wrong-schedule / wrong-network drift class.
    #[test]
    fn rejects_a_mismatched_height() {
        let reported = SAPLING_MAINNET_HEIGHT + 1;
        let upgrades = [upgrade(SAPLING, reported)];

        let reason =
            super::verify_reported_upgrades(&zebra_chain::parameters::Network::Mainnet, &upgrades)
                .expect_err("a wrong Sapling height must be rejected");
        assert!(
            reason.contains(&reported.to_string()),
            "error should name the reported height, got: {reason}"
        );
    }

    /// An upgrade the compiled schedule disables on this network must be
    /// rejected when the validator reports a height for it.
    #[test]
    fn rejects_an_upgrade_the_compiled_schedule_lacks() {
        let upgrades = [upgrade(NU7, 123)];

        let reason =
            super::verify_reported_upgrades(&zebra_chain::parameters::Network::Mainnet, &upgrades)
                .expect_err("an unscheduled upgrade with a reported height must be rejected");
        assert!(
            reason.contains("None"),
            "error should show the compiled schedule has no height, got: {reason}"
        );
    }

    /// A branch id no build knows is skipped, not rejected — forward-compat for
    /// a validator ahead of Zaino.
    #[test]
    fn skips_an_unknown_branch() {
        let upgrades = [upgrade(UNKNOWN_BRANCH, 123)];

        super::verify_reported_upgrades(&zebra_chain::parameters::Network::Mainnet, &upgrades)
            .expect("an unknown branch id must be skipped, not rejected");
    }
}
