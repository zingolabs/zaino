//! The network upgrade schedule, read against the served tip.
//!
//! `GetLightdInfo` reports three facts the validator's schedule determines,
//! each relative to the tip being served:
//!
//! ```text
//! branch_in_force(tip) = branch of max { u ∈ upgrades | activation(u) ≤ tip }
//! next_upgrade(tip)    = min { u ∈ upgrades | activation(u) > tip }
//! sapling_activation   = activation of the upgrade carrying Sapling's branch id
//! ```
//!
//! Upgrades the validator marks disabled are in neither order and are skipped.
//! Before the first tip is served only the Sapling activation is known.

use zaino_primitives::types::{
    ConsensusBranchId, Height, NetworkUpgradeInfo, NetworkUpgradeStatus, PoolActivations,
};

/// The schedule facts `GetLightdInfo` reports at one tip.
pub(crate) struct ScheduleAt<'a> {
    /// Where Sapling activates on this network, if it is scheduled.
    pub(crate) sapling_activation: Option<Height>,
    /// The consensus branch in force at the tip.
    pub(crate) branch_in_force: Option<ConsensusBranchId>,
    /// The first upgrade the chain has not reached yet.
    pub(crate) next_upgrade: Option<&'a NetworkUpgradeInfo>,
}

/// Read `upgrades` against `tip` (`None` before any block is served).
pub(crate) fn at(upgrades: &[NetworkUpgradeInfo], tip: Option<Height>) -> ScheduleAt<'_> {
    let scheduled = || {
        upgrades
            .iter()
            .filter(|upgrade| upgrade.status != NetworkUpgradeStatus::Disabled)
    };
    let sapling_activation = PoolActivations::from_branch_activations(
        scheduled().map(|upgrade| (u32::from(upgrade.branch_id), upgrade.activation_height)),
    )
    .sapling;
    let Some(tip) = tip else {
        return ScheduleAt {
            sapling_activation,
            branch_in_force: None,
            next_upgrade: None,
        };
    };
    ScheduleAt {
        sapling_activation,
        branch_in_force: scheduled()
            .filter(|upgrade| upgrade.activation_height <= tip)
            .max_by_key(|upgrade| upgrade.activation_height)
            .map(|upgrade| upgrade.branch_id),
        next_upgrade: scheduled()
            .filter(|upgrade| upgrade.activation_height > tip)
            .min_by_key(|upgrade| upgrade.activation_height),
    }
}

#[cfg(test)]
mod tests {
    use super::at;
    use zaino_primitives::types::{
        ConsensusBranchId, Height, NetworkUpgradeInfo, NetworkUpgradeStatus,
    };

    /// Sapling's consensus branch id.
    const SAPLING: u32 = 0x76b8_09bb;
    /// NU5's consensus branch id.
    const NU5: u32 = 0xc2d6_d0b4;
    /// NU6's consensus branch id.
    const NU6: u32 = 0xc8e7_1055;

    fn h(height: u32) -> Height {
        Height::try_from(height).expect("a valid test height")
    }

    fn upgrade(branch: u32, activation: u32, status: NetworkUpgradeStatus) -> NetworkUpgradeInfo {
        NetworkUpgradeInfo {
            branch_id: ConsensusBranchId::new(branch),
            name: format!("{branch:08x}"),
            activation_height: h(activation),
            status,
        }
    }

    fn mainnet_like() -> Vec<NetworkUpgradeInfo> {
        vec![
            upgrade(NU6, 2_726_400, NetworkUpgradeStatus::Pending),
            upgrade(SAPLING, 419_200, NetworkUpgradeStatus::Active),
            upgrade(NU5, 1_687_104, NetworkUpgradeStatus::Active),
        ]
    }

    /// Between two upgrades, the earlier one is in force and the later one is
    /// next, whatever order the validator listed them in.
    #[test]
    fn between_upgrades_the_earlier_is_in_force_and_the_later_is_next() {
        let upgrades = mainnet_like();
        let schedule = at(&upgrades, Some(h(2_000_000)));
        assert_eq!(schedule.sapling_activation, Some(h(419_200)));
        assert_eq!(schedule.branch_in_force, Some(ConsensusBranchId::new(NU5)));
        assert_eq!(
            schedule.next_upgrade.map(|upgrade| upgrade.branch_id),
            Some(ConsensusBranchId::new(NU6))
        );
    }

    /// An upgrade is in force from its activation height itself.
    #[test]
    fn an_upgrade_is_in_force_at_its_activation_height() {
        let upgrades = mainnet_like();
        let schedule = at(&upgrades, Some(h(2_726_400)));
        assert_eq!(schedule.branch_in_force, Some(ConsensusBranchId::new(NU6)));
        assert!(schedule.next_upgrade.is_none());
    }

    /// A disabled upgrade is neither in force nor next.
    #[test]
    fn a_disabled_upgrade_is_skipped() {
        let upgrades = vec![
            upgrade(SAPLING, 1, NetworkUpgradeStatus::Active),
            upgrade(NU5, 10, NetworkUpgradeStatus::Disabled),
        ];
        let schedule = at(&upgrades, Some(h(20)));
        assert_eq!(
            schedule.branch_in_force,
            Some(ConsensusBranchId::new(SAPLING))
        );
        assert!(at(&upgrades, Some(h(5))).next_upgrade.is_none());
    }

    /// Before any tip is served only the activation height is known.
    #[test]
    fn without_a_tip_only_sapling_activation_is_reported() {
        let upgrades = mainnet_like();
        let schedule = at(&upgrades, None);
        assert_eq!(schedule.sapling_activation, Some(h(419_200)));
        assert!(schedule.branch_in_force.is_none());
        assert!(schedule.next_upgrade.is_none());
    }
}
