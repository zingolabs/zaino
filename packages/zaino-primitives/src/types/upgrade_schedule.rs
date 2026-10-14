//! Network-upgrade activation heights

use zcash_protocol::consensus::{
    BranchId, MainNetwork, NetworkType, NetworkUpgrade, Parameters, TestNetwork,
};

use super::{BlockchainInfo, ConsensusBranchId};
use zcash_protocol::consensus::BlockHeight;

const PRE_BLOSSOM_SPACING: u32 = 150;
const POST_BLOSSOM_SPACING: u32 = 75;
/// ZIP 218
const POST_NU7_SPACING: u32 = 25;

/// Protocol order (`NetworkUpgrade` derives neither `Ord` nor `Hash`)
pub(crate) const ORDER: [NetworkUpgrade; 11] = [
    NetworkUpgrade::Overwinter,
    NetworkUpgrade::Sapling,
    NetworkUpgrade::Blossom,
    NetworkUpgrade::Heartwood,
    NetworkUpgrade::Canopy,
    NetworkUpgrade::Nu5,
    NetworkUpgrade::Nu6,
    NetworkUpgrade::Nu6_1,
    NetworkUpgrade::Nu6_2,
    NetworkUpgrade::Nu6_3,
    NetworkUpgrade::Nu7,
];

/// Activation height per upgrade (`None` = never), indexed by `ORDER`
///
/// - Mainnet/testnet: compiled in (`zcash_protocol`'s tables, as zebrad's); regtest: its validator's
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpgradeSchedule(pub(crate) [Option<BlockHeight>; ORDER.len()]);

impl UpgradeSchedule {
    /// `None` = regtest (heights set per chain: [`reported`](Self::reported) by its validator)
    pub fn compiled(network: NetworkType) -> Option<Self> {
        match network {
            NetworkType::Main => Some(Self::of(&MainNetwork)),
            NetworkType::Test => Some(Self::of(&TestNetwork)),
            NetworkType::Regtest => None,
        }
    }

    /// `getblockchaininfo` `upgrades`, keyed by branch id
    pub fn reported(info: &BlockchainInfo) -> Self {
        Self(ORDER.map(|upgrade| {
            let branch_id = ConsensusBranchId::of(upgrade);
            let reported = info.upgrades.iter().find(|nu| nu.branch_id == branch_id);
            reported.map(|nu| nu.activation_height)
        }))
    }

    pub fn activation(&self, upgrade: NetworkUpgrade) -> Option<BlockHeight> {
        self.0[slot(upgrade)]
    }

    pub fn active(&self, upgrade: NetworkUpgrade, at: BlockHeight) -> bool {
        self.activation(upgrade).is_some_and(|from| from <= at)
    }

    /// Branch in force at `at`
    pub fn branch_at(&self, at: BlockHeight) -> ConsensusBranchId {
        let latest = ORDER.iter().rev().find(|upgrade| self.active(**upgrade, at));
        ConsensusBranchId::from(latest.map_or(BranchId::Sprout, |upgrade| upgrade.branch_id()))
    }

    /// First upgrade scheduled above `at`
    pub fn next_after(&self, at: BlockHeight) -> Option<(NetworkUpgrade, BlockHeight)> {
        self.scheduled().filter(|(_, from)| *from > at).min_by_key(|(_, from)| *from)
    }

    /// `PoWTargetSpacing(at)`, seconds
    pub fn spacing(&self, at: BlockHeight) -> u32 {
        match (self.active(NetworkUpgrade::Nu7, at), self.active(NetworkUpgrade::Blossom, at)) {
            (true, _) => POST_NU7_SPACING,
            (false, true) => POST_BLOSSOM_SPACING,
            (false, false) => PRE_BLOSSOM_SPACING,
        }
    }

    /// Each scheduled upgrade + its height, protocol order
    pub(crate) fn scheduled(&self) -> impl Iterator<Item = (NetworkUpgrade, BlockHeight)> + '_ {
        ORDER.iter().zip(self.0).filter_map(|(upgrade, at)| Some((*upgrade, at?)))
    }

    fn of(params: &impl Parameters) -> Self {
        Self(ORDER.map(|upgrade| params.activation_height(upgrade)))
    }
}

pub(crate) fn slot(upgrade: NetworkUpgrade) -> usize {
    ORDER.iter().position(|known| *known == upgrade).expect("ORDER lists every enabled upgrade")
}

#[cfg(test)]
mod tests {
    use NetworkUpgrade::*;

    use super::*;
    use crate::testing::h;
    use crate::types::{
        BlockHash, ConsensusBranchId, ConsensusBranchIds, NetworkUpgradeInfo, NetworkUpgradeStatus,
    };

    /// Compiled tables = the live chains' heights; regtest has none; a validator's report keys on
    /// branch id (unlisted = never, pending = its scheduled height) and round-trips a schedule
    #[test]
    fn compiled_tables_hold_the_live_heights_and_reports_round_trip() {
        let mainnet = UpgradeSchedule::compiled(NetworkType::Main).expect("mainnet compiled in");
        let testnet = UpgradeSchedule::compiled(NetworkType::Test).expect("testnet compiled in");
        assert_eq!(UpgradeSchedule::compiled(NetworkType::Regtest), None);

        // mainnet: a mainnet zebrad's getblockchaininfo; blossom + NU7: the header rules' heights
        let at = |schedule: &UpgradeSchedule, upgrade| schedule.activation(upgrade);
        assert_eq!(
            [Sapling, Blossom, Nu5, Nu6, Nu6_3, Nu7].map(|upgrade| at(&mainnet, upgrade)),
            [
                Some(h(419_200)),
                Some(h(653_600)),
                Some(h(1_687_104)),
                Some(h(2_726_400)),
                Some(h(3_428_143)),
                None
            ],
        );
        assert_eq!(
            [Blossom, Nu7].map(|upgrade| at(&testnet, upgrade)),
            [Some(h(584_000)), Some(h(4_465_026))],
        );

        let report = |schedule: &UpgradeSchedule, tip: u32| BlockchainInfo {
            blocks: h(tip),
            estimated_height: h(tip),
            best_block_hash: BlockHash::from([0; 32]),
            sapling_activation: at(schedule, Sapling).expect("sapling"),
            upgrades: schedule
                .scheduled()
                .collect::<Vec<_>>()
                .into_iter()
                .rev() // listed in no particular order: keyed by branch id
                .map(|(upgrade, activation_height)| NetworkUpgradeInfo {
                    branch_id: ConsensusBranchId::of(upgrade),
                    name: "label only".to_owned(),
                    activation_height,
                    status: match activation_height <= h(tip) {
                        true => NetworkUpgradeStatus::Active,
                        false => NetworkUpgradeStatus::Pending,
                    },
                })
                .collect(),
            consensus: ConsensusBranchIds {
                chain_tip: ConsensusBranchId::new(0),
                next_block: ConsensusBranchId::new(0),
            },
        };
        // testnet below its NU7 height: NU7 reported pending, still at its height
        assert_eq!(UpgradeSchedule::reported(&report(&testnet, 4_000_000)), testnet);
        assert_eq!(UpgradeSchedule::reported(&report(&mainnet, 3_500_000)), mainnet);

        let mut no_nu5 = report(&mainnet, 3_500_000);
        no_nu5.upgrades.retain(|upgrade| upgrade.branch_id != ConsensusBranchId::of(Nu5));
        assert_eq!(UpgradeSchedule::reported(&no_nu5).activation(Nu5), None, "unlisted = never");
    }

    /// Mainnet around NU6.3 (3,428,143): active from its height on, the branch switches there, and
    /// the next upgrade is strictly above (none past the last scheduled); below Overwinter = Sprout
    #[test]
    fn the_branch_and_next_upgrade_switch_exactly_at_an_activation_height() {
        let mainnet = UpgradeSchedule::compiled(NetworkType::Main).expect("mainnet compiled in");
        let branch = ConsensusBranchId::of;
        let read = |at: u32| {
            let at = h(at);
            (mainnet.active(Nu6_3, at), mainnet.branch_at(at), mainnet.next_after(at))
        };
        assert_eq!(read(3_428_142), (false, branch(Nu6_2), Some((Nu6_3, h(3_428_143)))));
        assert_eq!(read(3_428_143), (true, branch(Nu6_3), None));
        assert_eq!(read(3_500_000), (true, branch(Nu6_3), None));
        assert_eq!(read(0), (false, ConsensusBranchId::new(0), Some((Overwinter, h(347_500)))));
        assert_eq!(mainnet.next_after(h(347_500)), Some((Sapling, h(419_200))));
    }
}
