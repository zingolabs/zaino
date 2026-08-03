//! The `getblockchaininfo` response.
//!
//! Reuses Zebra's `GetBlockchainInfoResponse`, so this module holds only the
//! conversion from the domain — but that conversion is the largest in the wire
//! layer, because this response reshapes value pools into a fixed array and
//! renames network upgrades by consensus branch id.

use zaino_primitives::types::{BlockchainInfo, ValuePoolBalance};
use zebra_chain::parameters::Network;
use zebra_rpc::methods::GetBlockchainInfoResponse;

/// A `getblockchaininfo` field the wire type cannot represent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockchainInfoWireError {
    /// A value pool the interface has no slot for.
    #[error("unknown value pool `{0}`")]
    UnknownValuePool(String),

    /// A pool balance outside the range the interface's amount type allows.
    #[error("value pool balance out of range: {0}")]
    PoolBalanceOutOfRange(String),

    /// A consensus branch id this build does not recognise.
    ///
    /// Rejected rather than guessed: Zaino adopts the validator's activation
    /// schedule, and a wrong entry would put it on different consensus rules
    /// from the validator it indexes.
    #[error("validator reported consensus branch {0}, which this build does not recognise")]
    UnrecognisedConsensusBranch(String),
}

/// One value pool as the interface's balance type, keyed by pool name.
///
/// The name is how the interface identifies a pool, so an unrecognised one is
/// rejected rather than silently filed under the wrong pool.
fn pool_balance(
    balance: &ValuePoolBalance,
) -> Result<zebra_rpc::client::GetBlockchainInfoBalance, BlockchainInfoWireError> {
    use zebra_rpc::client::GetBlockchainInfoBalance;

    fn amount<C: zebra_chain::amount::Constraint>(
        zats: i64,
    ) -> Result<zebra_chain::amount::Amount<C>, BlockchainInfoWireError> {
        zebra_chain::amount::Amount::try_from(zats)
            .map_err(|e| BlockchainInfoWireError::PoolBalanceOutOfRange(e.to_string()))
    }

    let value = amount(
        i64::try_from(u64::from(balance.chain_value))
            .map_err(|e| BlockchainInfoWireError::PoolBalanceOutOfRange(e.to_string()))?,
    )?;
    let delta = balance
        .value_delta
        .map(|d| amount(i64::from(d)))
        .transpose()?;

    Ok(match balance.id.as_str() {
        "transparent" => GetBlockchainInfoBalance::transparent(value, delta),
        "sprout" => GetBlockchainInfoBalance::sprout(value, delta),
        "sapling" => GetBlockchainInfoBalance::sapling(value, delta),
        "orchard" => GetBlockchainInfoBalance::orchard(value, delta),
        // zebra names this pool `lockbox` on the wire; `deferred` is zcashd's
        // name for the same pool, and zebra's own constructor is still called
        // `deferred`. Both spellings are accepted so the answer does not depend
        // on which validator is behind the adapter.
        "lockbox" | "deferred" => GetBlockchainInfoBalance::deferred(value, delta),
        "ironwood" => GetBlockchainInfoBalance::ironwood(value, delta),
        // `chainSupply` is a total rather than a pool, and arrives unnamed.
        "" => GetBlockchainInfoBalance::chain_supply(Default::default()),
        other => return Err(BlockchainInfoWireError::UnknownValuePool(other.to_string())),
    })
}

/// The interface reports value pools as a fixed six-slot array, in a defined
/// order. Pools the validator did not report are zero rather than absent —
/// there is no slot for "unknown".
fn value_pool_array(
    pools: &[ValuePoolBalance],
) -> Result<zebra_rpc::methods::BlockchainValuePoolBalances, BlockchainInfoWireError> {
    let mut slots = zebra_rpc::client::GetBlockchainInfoBalance::zero_pools();
    for pool in pools {
        let built = pool_balance(pool)?;
        let slot = match pool.id.as_str() {
            "transparent" => 0,
            "sprout" => 1,
            "sapling" => 2,
            "orchard" => 3,
            "lockbox" | "deferred" => 4,
            "ironwood" => 5,
            other => return Err(BlockchainInfoWireError::UnknownValuePool(other.to_string())),
        };
        slots[slot] = built;
    }
    Ok(slots)
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
    use zebra_rpc::methods::{
        ConsensusBranchIdHex, NetworkUpgradeInfo, NetworkUpgradeStatus, TipConsensusBranch,
    };

    let upgrades: indexmap::IndexMap<_, _> = info
        .upgrades
        .into_iter()
        .map(|upgrade| {
            let branch =
                zebra_chain::parameters::ConsensusBranchId::from(u32::from(upgrade.branch_id));
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
                ConsensusBranchIdHex::new(branch.into()),
                NetworkUpgradeInfo::from_parts(
                    named,
                    zebra_chain::block::Height(upgrade.activation_height.into()),
                    status,
                ),
            ))
        })
        .collect::<Result<_, BlockchainInfoWireError>>()?;

    Ok(GetBlockchainInfoResponse::new(
        info.chain,
        zebra_chain::block::Height(info.blocks.into()),
        zebra_chain::block::Hash(info.best_block_hash.into()),
        zebra_chain::block::Height(info.estimated_height.into()),
        pool_balance(&info.chain_supply)?,
        value_pool_array(&info.value_pools)?,
        upgrades,
        TipConsensusBranch::from_parts(
            ConsensusBranchIdHex::new(u32::from(info.consensus.chain_tip)).inner(),
            ConsensusBranchIdHex::new(u32::from(info.consensus.next_block)).inner(),
        ),
        zebra_chain::block::Height(info.headers.into()),
        info.difficulty,
        info.verification_progress,
        // The interface types cumulative work as a 64-bit integer, which cannot
        // hold a real mainnet value. The domain reports `None` where the
        // validator does not track it; zero is what this field has always
        // carried in that case.
        0,
        info.pruned,
        info.size_on_disk,
        info.commitments,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{
        BlockHash, BlockchainInfo, ConsensusBranchIds, Height, NetworkUpgradeInfo,
        NetworkUpgradeStatus, Zatoshis,
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

    /// An unnamed pool is `chainSupply`, a total rather than a pool. Filing it
    /// as one would double-count.
    #[test]
    fn an_unrecognised_pool_name_is_rejected() {
        let mut info = sample();
        info.value_pools.push(pool("plasma", 1));

        assert_eq!(
            from_domain(info, &Network::new_regtest(Default::default())),
            Err(BlockchainInfoWireError::UnknownValuePool(
                "plasma".to_string()
            ))
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
