//! The `getblockchaininfo` response and its conversion from the domain — the
//! largest conversion in the wire layer, because this response reshapes value
//! pools into a fixed array and renames network upgrades by consensus branch id.

use zaino_primitives::types::BlockchainInfo;
use zaino_state::jsonrpc_types::{
    BlockchainValuePoolBalances, GetBlockchainInfoBalance, UnknownValuePool,
};
use zebra_chain::{
    block,
    parameters::{ConsensusBranchId, Network, NetworkUpgrade},
};

/// The `getblockchaininfo` response.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct GetBlockchainInfoResponse {
    /// The network name as BIP70 defines it: main, test or regtest.
    chain: String,
    /// The number of blocks the server has processed.
    blocks: block::Height,
    /// The number of headers validated in the best chain.
    headers: block::Height,
    /// The estimated network solution rate in Sol/s.
    difficulty: f64,
    /// The verification progress relative to the estimated network chain tip.
    #[serde(rename = "verificationprogress")]
    verification_progress: f64,
    /// The total amount of work in the best chain.
    #[serde(rename = "chainwork")]
    chain_work: u64,
    /// Whether this node is pruned.
    pruned: bool,
    /// The estimated size of the block and undo files on disk.
    size_on_disk: u64,
    /// The current number of note commitments in the commitment tree.
    commitments: u64,
    /// The hash of the best block, as display-order hex.
    #[serde(rename = "bestblockhash", with = "hex")]
    best_block_hash: block::Hash,
    /// The estimated height of the chain when syncing, else the best height.
    #[serde(rename = "estimatedheight")]
    estimated_height: block::Height,
    /// The chain supply balance.
    #[serde(rename = "chainSupply")]
    chain_supply: GetBlockchainInfoBalance,
    /// The value pool balances.
    #[serde(rename = "valuePools")]
    value_pools: BlockchainValuePoolBalances,
    /// The status of each network upgrade, keyed by consensus branch id.
    upgrades: indexmap::IndexMap<ConsensusBranchIdHex, NetworkUpgradeInfo>,
    /// The branch ids of the current and upcoming consensus rules.
    consensus: TipConsensusBranch,
}

/// A consensus branch id that serializes as big-endian hex.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub struct ConsensusBranchIdHex(#[serde(with = "hex")] ConsensusBranchId);

/// The activation of one network upgrade.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct NetworkUpgradeInfo {
    /// The upgrade's name.
    name: NetworkUpgrade,
    /// The upgrade's activation height.
    #[serde(rename = "activationheight")]
    activation_height: block::Height,
    /// The upgrade's activation status.
    status: NetworkUpgradeStatus,
}

/// The activation status of a network upgrade.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub enum NetworkUpgradeStatus {
    /// The upgrade has activated, whether or not it is the most recent one.
    #[serde(rename = "active")]
    Active,
    /// The upgrade has no activation height.
    #[serde(rename = "disabled")]
    Disabled,
    /// The upgrade has an activation height the chain has not reached.
    #[serde(rename = "pending")]
    Pending,
}

/// The consensus branch ids for the tip and for the next block.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct TipConsensusBranch {
    /// The branch id that validates the current chain tip.
    #[serde(rename = "chaintip")]
    chain_tip: ConsensusBranchIdHex,
    /// The branch id that validates the next block.
    #[serde(rename = "nextblock")]
    next_block: ConsensusBranchIdHex,
}

/// A `getblockchaininfo` field the wire type cannot represent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockchainInfoWireError {
    /// A value pool the interface has no slot for.
    #[error(transparent)]
    UnknownValuePool(#[from] UnknownValuePool),

    /// A consensus branch id this build does not recognise.
    ///
    /// Rejected rather than guessed: Zaino adopts the validator's activation
    /// schedule, and a wrong entry would put it on different consensus rules
    /// from the validator it indexes.
    #[error("validator reported consensus branch {0}, which this build does not recognise")]
    UnrecognisedConsensusBranch(String),
}

/// Renders the domain type as the `getblockchaininfo` response.
///
/// `network` is needed because the two vocabularies name upgrades differently:
/// the interface names them by their enum variant, the domain by their
/// consensus branch id — the protocol-defined identity. There is no direct
/// conversion, so the network's own activation list is the lookup.
pub fn from_domain(
    info: BlockchainInfo,
    network: &Network,
) -> Result<GetBlockchainInfoResponse, BlockchainInfoWireError> {
    let upgrades: indexmap::IndexMap<_, _> = info
        .upgrades
        .into_iter()
        .map(|upgrade| {
            let branch = ConsensusBranchId::from(u32::from(upgrade.branch_id));
            let status = match upgrade.status {
                zaino_primitives::types::NetworkUpgradeStatus::Active => {
                    NetworkUpgradeStatus::Active
                }
                zaino_primitives::types::NetworkUpgradeStatus::Pending => {
                    NetworkUpgradeStatus::Pending
                }
                zaino_primitives::types::NetworkUpgradeStatus::Disabled => {
                    NetworkUpgradeStatus::Disabled
                }
            };
            let named = network
                .full_activation_list()
                .into_iter()
                .find_map(|(_height, upgrade)| {
                    (upgrade.branch_id() == Some(branch)).then_some(upgrade)
                })
                .ok_or_else(|| {
                    BlockchainInfoWireError::UnrecognisedConsensusBranch(format!("{branch:?}"))
                })?;
            Ok((
                ConsensusBranchIdHex(branch),
                NetworkUpgradeInfo {
                    name: named,
                    activation_height: block::Height(upgrade.activation_height.into()),
                    status,
                },
            ))
        })
        .collect::<Result<_, BlockchainInfoWireError>>()?;

    Ok(GetBlockchainInfoResponse {
        chain: info.chain,
        blocks: block::Height(info.blocks.into()),
        best_block_hash: block::Hash(info.best_block_hash.into()),
        estimated_height: block::Height(info.estimated_height.into()),
        chain_supply: GetBlockchainInfoBalance::from_domain(&info.chain_supply)?,
        value_pools: GetBlockchainInfoBalance::value_pools_from_domain(&info.value_pools)?,
        upgrades,
        consensus: TipConsensusBranch {
            chain_tip: ConsensusBranchIdHex(u32::from(info.consensus.chain_tip).into()),
            next_block: ConsensusBranchIdHex(u32::from(info.consensus.next_block).into()),
        },
        headers: block::Height(info.headers.into()),
        difficulty: info.difficulty,
        verification_progress: info.verification_progress,
        // The interface types cumulative work as a 64-bit integer, which cannot
        // hold a real mainnet value. The domain reports `None` where the
        // validator does not track it; zero is what this field has always
        // carried in that case.
        chain_work: 0,
        pruned: info.pruned,
        size_on_disk: info.size_on_disk,
        commitments: info.commitments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{
        BlockHash, BlockchainInfo, ConsensusBranchIds, Height, NetworkUpgradeInfo,
        NetworkUpgradeStatus, ValuePoolBalance, Zatoshis,
    };

    fn pool(id: &str, value: u64) -> ValuePoolBalance {
        ValuePoolBalance {
            id: id.to_string(),
            chain_value: Zatoshis::new(value).unwrap(),
            monitored: true,
            value_delta: None,
        }
    }

    fn sample() -> BlockchainInfo {
        BlockchainInfo {
            chain: "regtest".to_string(),
            blocks: Height::try_from(100u32).unwrap(),
            headers: Height::try_from(100u32).unwrap(),
            estimated_height: Height::try_from(100u32).unwrap(),
            best_block_hash: BlockHash::from([0x11; 32]),
            difficulty: 1.0,
            verification_progress: 1.0,
            chain_work: None,
            pruned: false,
            size_on_disk: 4_096,
            commitments: 7,
            chain_supply: pool("", 0),
            value_pools: vec![pool("transparent", 1_000), pool("orchard", 2_000)],
            upgrades: Vec::new(),
            consensus: ConsensusBranchIds {
                chain_tip: 0x7761_0b1e.into(),
                next_block: 0x7761_0b1e.into(),
            },
        }
    }

    /// Pools the validator did not report occupy their slot as zero. The
    /// interface has no way to say "unknown", so the array is always six long
    /// and always in the same order.
    #[test]
    fn unreported_pools_are_zero_not_absent() {
        let wire = from_domain(sample(), &Network::new_regtest(Default::default()))
            .expect("sample renders");
        let json = serde_json::to_value(&wire).unwrap();

        let pools = json["valuePools"].as_array().expect("an array of pools");
        assert_eq!(pools.len(), 6, "the array is fixed at six slots");
        assert_eq!(pools[0]["id"], "transparent");
        assert_eq!(pools[0]["chainValue"], 0.00001);
        assert_eq!(pools[1]["id"], "sprout");
        assert_eq!(pools[1]["chainValueZat"], 0);
    }

    /// `chainSupply` carries the validator's total, not a zero. Discarding it
    /// reported every chain as holding nothing.
    #[test]
    fn chain_supply_carries_its_value() {
        let mut info = sample();
        info.chain_supply = pool("", 3_000);

        let wire =
            from_domain(info, &Network::new_regtest(Default::default())).expect("sample renders");
        let json = serde_json::to_value(&wire).unwrap();

        assert_eq!(json["chainSupply"]["chainValueZat"], 3_000);
        assert_eq!(json["chainSupply"]["monitored"], true);
        assert!(
            json["chainSupply"].get("id").is_none(),
            "a total is unnamed: {}",
            json["chainSupply"]
        );
    }

    /// An unnamed pool is `chainSupply`, a total rather than a pool. Filing it
    /// as one would double-count.
    #[test]
    fn an_unrecognised_pool_name_is_rejected() {
        let mut info = sample();
        info.value_pools.push(pool("plasma", 1));

        assert_eq!(
            from_domain(info, &Network::new_regtest(Default::default())),
            Err(BlockchainInfoWireError::UnknownValuePool(UnknownValuePool(
                "plasma".to_string()
            )))
        );
    }

    /// Zaino adopts the validator's activation schedule as its own consensus
    /// rules, so an upgrade this build cannot name is an error rather than a
    /// guess.
    #[test]
    fn an_unrecognised_consensus_branch_is_rejected() {
        let mut info = sample();
        info.upgrades.push(NetworkUpgradeInfo {
            branch_id: 0xdead_beefu32.into(),
            name: "Nonexistent".to_string(),
            activation_height: Height::try_from(1u32).unwrap(),
            status: NetworkUpgradeStatus::Active,
        });

        assert!(matches!(
            from_domain(info, &Network::new_regtest(Default::default())),
            Err(BlockchainInfoWireError::UnrecognisedConsensusBranch(_))
        ));
    }

    /// `chainwork` is emitted as zero, not omitted: the interface types it as a
    /// `u64`, too narrow for a real mainnet value, so the domain does not carry
    /// one and this field has always been a placeholder.
    #[test]
    fn chainwork_is_the_documented_placeholder() {
        let wire = from_domain(sample(), &Network::new_regtest(Default::default()))
            .expect("sample renders");
        let json = serde_json::to_value(&wire).unwrap();

        assert_eq!(json["chainwork"], 0);
    }
}
