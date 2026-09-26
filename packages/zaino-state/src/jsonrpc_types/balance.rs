use zaino_primitives::types::{SignedZatoshis, ValuePoolBalance, Zatoshis};

/// Zatoshis per ZEC.
const ZATOSHIS_PER_ZEC: f64 = 100_000_000.0;

/// The pool names in the order the six value-pool slots list them.
const POOL_IDS: [&str; 6] = [
    "transparent",
    "sprout",
    "sapling",
    "orchard",
    "lockbox",
    "ironwood",
];

/// Converts zatoshis to the lossy ZEC float this interface reports, which consensus-critical code must never use.
pub fn zatoshis_to_lossy_zec(zatoshis: i64) -> f64 {
    zatoshis as f64 / ZATOSHIS_PER_ZEC
}

/// A value pool name the interface has no slot for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown value pool `{0}`")]
pub struct UnknownValuePool(pub String);

/// The slot a pool name occupies, `None` for the unnamed chain supply, accepting zcashd's `deferred` for the lockbox.
fn pool_slot(id: &str) -> Result<Option<usize>, UnknownValuePool> {
    match id {
        "" => Ok(None),
        "deferred" => Ok(Some(4)),
        other => POOL_IDS
            .iter()
            .position(|pool| *pool == other)
            .map(Some)
            .ok_or_else(|| UnknownValuePool(other.to_string())),
    }
}

/// A value pool's balance in ZEC and zatoshis.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetBlockchainInfoBalance {
    /// The pool's name, empty for the chain supply.
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    /// The total amount in the pool, in ZEC.
    chain_value: f64,
    /// The total amount in the pool, in zatoshis.
    chain_value_zat: u64,
    /// Whether the pool holds any value.
    monitored: bool,
    /// The change to the pool's amount produced by the block, in ZEC.
    #[serde(skip_serializing_if = "Option::is_none")]
    value_delta: Option<f64>,
    /// The change to the pool's amount produced by the block, in zatoshis.
    #[serde(skip_serializing_if = "Option::is_none")]
    value_delta_zat: Option<i64>,
}

impl GetBlockchainInfoBalance {
    /// Builds the balance of the pool named `id`.
    fn new(id: &str, value: Zatoshis, delta: Option<SignedZatoshis>) -> Self {
        let value = u64::from(value);
        Self {
            id: id.to_string(),
            chain_value: zatoshis_to_lossy_zec(value as i64),
            chain_value_zat: value,
            monitored: value != 0,
            value_delta: delta.map(|delta| zatoshis_to_lossy_zec(i64::from(delta))),
            value_delta_zat: delta.map(i64::from),
        }
    }

    /// The chain supply when the validator reported none.
    pub fn empty_chain_supply() -> Self {
        Self::new("", Zatoshis::ZERO, None)
    }

    /// Renders a domain balance, keyed by its pool name, with an unnamed balance as the chain supply.
    pub fn from_domain(balance: &ValuePoolBalance) -> Result<Self, UnknownValuePool> {
        Ok(match pool_slot(&balance.id)? {
            None => Self::new("", balance.chain_value, None),
            Some(slot) => Self::new(POOL_IDS[slot], balance.chain_value, balance.value_delta),
        })
    }

    /// Renders domain balances as the six value-pool slots, with a pool the validator did not report as zero.
    pub fn value_pools_from_domain(
        pools: &[ValuePoolBalance],
    ) -> Result<BlockchainValuePoolBalances, UnknownValuePool> {
        let mut slots = POOL_IDS.map(|id| Self::new(id, Zatoshis::ZERO, None));
        for pool in pools {
            let slot = pool_slot(&pool.id)?.ok_or_else(|| UnknownValuePool(pool.id.clone()))?;
            slots[slot] = Self::new(POOL_IDS[slot], pool.chain_value, pool.value_delta);
        }
        Ok(slots)
    }
}

/// The six value-pool balances, in the order the interface lists them.
pub type BlockchainValuePoolBalances = [GetBlockchainInfoBalance; 6];
