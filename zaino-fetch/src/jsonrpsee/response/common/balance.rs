//! Types used to represent a value pool's balance.

use std::convert::Infallible;

use serde::{de, Deserialize, Deserializer, Serialize};
use zebra_chain::{
    amount::{Amount, NonNegative},
    value_balance::ValueBalance,
};
use zebra_rpc::client::GetBlockchainInfoBalance;

use crate::jsonrpsee::connector::ResponseToError;

/// Wrapper struct for a Zebra [`GetBlockchainInfoBalance`], enabling custom
/// deserialisation logic to handle both zebrad and zcashd.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChainBalance(GetBlockchainInfoBalance);

impl ChainBalance {
    /// Borrow the wrapped [`GetBlockchainInfoBalance`].
    pub fn as_inner(&self) -> &GetBlockchainInfoBalance {
        &self.0
    }

    /// Borrow the wrapped [`GetBlockchainInfoBalance`] mutably.
    pub fn as_inner_mut(&mut self) -> &mut GetBlockchainInfoBalance {
        &mut self.0
    }

    /// Consume [`self`] and return the wrapped [`GetBlockchainInfoBalance`].
    pub fn into_inner(self) -> GetBlockchainInfoBalance {
        self.0
    }
}

impl ResponseToError for ChainBalance {
    type RpcError = Infallible;
}

impl<'de> Deserialize<'de> for ChainBalance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize, Debug)]
        struct TempBalance {
            #[serde(default)]
            id: String,
            #[serde(rename = "chainValue")]
            chain_value: f64,
            #[serde(rename = "chainValueZat")]
            chain_value_zat: u64,
            #[allow(dead_code)]
            #[serde(default)]
            monitored: bool,
        }
        let temp = TempBalance::deserialize(deserializer)?;
        let computed_zat = (temp.chain_value * 100_000_000.0).round() as u64;
        if computed_zat != temp.chain_value_zat {
            return Err(de::Error::custom(format!(
                "chainValue and chainValueZat mismatch: computed {} but got {}",
                computed_zat, temp.chain_value_zat
            )));
        }
        let amount = Amount::<NonNegative>::from_bytes(temp.chain_value_zat.to_le_bytes())
            .map_err(|e| de::Error::custom(e.to_string()))?;
        match temp.id.as_str() {
            "transparent" => Ok(ChainBalance(GetBlockchainInfoBalance::transparent(
                amount, None, /*TODO: handle optional delta*/
            ))),
            "sprout" => Ok(ChainBalance(GetBlockchainInfoBalance::sprout(
                amount, None, /*TODO: handle optional delta*/
            ))),
            "sapling" => Ok(ChainBalance(GetBlockchainInfoBalance::sapling(
                amount, None, /*TODO: handle optional delta*/
            ))),
            "orchard" => Ok(ChainBalance(GetBlockchainInfoBalance::orchard(
                amount, None, /*TODO: handle optional delta*/
            ))),
            // TODO: Investigate source of undocument 'lockbox' value
            // that likely is intended to be 'deferred'
            "lockbox" | "deferred" => Ok(ChainBalance(GetBlockchainInfoBalance::deferred(
                amount, None,
            ))),
            "" => Ok(ChainBalance(GetBlockchainInfoBalance::chain_supply(
                // The pools are immediately summed internally, which pool we pick doesn't matter here
                ValueBalance::from_transparent_amount(amount),
            ))),
            otherwise => todo!("error: invalid chain id deser {otherwise}"),
        }
    }
}

impl Default for ChainBalance {
    fn default() -> Self {
        Self(GetBlockchainInfoBalance::chain_supply(ValueBalance::zero()))
    }
}
