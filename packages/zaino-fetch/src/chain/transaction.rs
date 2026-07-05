//! Transaction fetching and deserialization functionality.

use crate::{chain::error::ParseError, utils::ParseFromSlice};
use std::{io::Cursor, sync::Arc};
use zaino_proto::proto::{
    compact_formats::{
        CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTx, CompactTxIn,
        TxOut as CompactTxOut,
    },
    utils::PoolTypeFilter,
};
use zebra_chain::{
    parameters::{OVERWINTER_VERSION_GROUP_ID, SAPLING_VERSION_GROUP_ID, TX_V5_VERSION_GROUP_ID},
    serialization::{ZcashDeserialize as _, ZcashSerialize as _},
    transparent,
};

/// Zingo-Indexer struct for a full zcash transaction.
#[derive(Debug, Clone)]
pub struct FullTransaction {
    /// Parsed Zebra transaction.
    transaction: Arc<zebra_chain::transaction::Transaction>,

    /// Raw transaction bytes.
    raw_bytes: Vec<u8>,

    /// Transaction Id, fetched using get_block JsonRPC with verbose = 1 when available.
    tx_id: Vec<u8>,
}

impl ParseFromSlice for FullTransaction {
    fn parse_from_slice(
        data: &[u8],
        txid: Option<Vec<Vec<u8>>>,
        tx_version: Option<u32>,
    ) -> Result<(&[u8], Self), ParseError> {
        if tx_version.is_some() {
            return Err(ParseError::InvalidData(
                "tx_version must be None for FullTransaction::parse_from_slice".to_string(),
            ));
        }

        let mut cursor = Cursor::new(data);
        let transaction = zebra_chain::transaction::Transaction::zcash_deserialize(&mut cursor)?;
        let consumed = usize::try_from(cursor.position())?;
        let tx_id = txid
            .and_then(|txids| txids.into_iter().next())
            .unwrap_or_else(|| transaction.hash().0.to_vec());

        Ok((
            &data[consumed..],
            Self {
                transaction: Arc::new(transaction),
                raw_bytes: data[..consumed].to_vec(),
                tx_id,
            },
        ))
    }
}

impl FullTransaction {
    pub(super) fn from_zebra(
        transaction: Arc<zebra_chain::transaction::Transaction>,
        tx_id: Option<Vec<u8>>,
    ) -> Result<Self, ParseError> {
        let raw_bytes = transaction.zcash_serialize_to_vec()?;
        let tx_id = tx_id.unwrap_or_else(|| transaction.hash().0.to_vec());

        Ok(Self {
            transaction,
            raw_bytes,
            tx_id,
        })
    }

    /// Returns overwintered bool.
    pub fn f_overwintered(&self) -> bool {
        self.transaction.is_overwintered()
    }

    /// Returns the transaction version.
    pub fn version(&self) -> u32 {
        self.transaction.version()
    }

    /// Returns the transaction version group id.
    pub fn n_version_group_id(&self) -> Option<u32> {
        match self.version() {
            3 => Some(OVERWINTER_VERSION_GROUP_ID),
            4 => Some(SAPLING_VERSION_GROUP_ID),
            5 => Some(TX_V5_VERSION_GROUP_ID),
            6 => Some(zebra_chain::parameters::TX_V6_VERSION_GROUP_ID),
            _ => None,
        }
    }

    /// Returns the consensus branch id of the transaction.
    pub fn consensus_branch_id(&self) -> u32 {
        self.transaction
            .network_upgrade()
            .and_then(|network_upgrade| network_upgrade.branch_id())
            .map(u32::from)
            .unwrap_or(0)
    }

    /// Returns a vec of transparent inputs: (prev_txid, prev_index, script_sig).
    pub fn transparent_inputs(&self) -> Vec<(Vec<u8>, u32, Vec<u8>)> {
        self.transaction
            .inputs()
            .iter()
            .map(|input| match input {
                transparent::Input::PrevOut {
                    outpoint,
                    unlock_script,
                    ..
                } => (
                    outpoint.hash.0.to_vec(),
                    outpoint.index,
                    unlock_script.as_raw_bytes().to_vec(),
                ),
                transparent::Input::Coinbase { .. } => (
                    vec![0; 32],
                    u32::MAX,
                    input.coinbase_script().unwrap_or_default(),
                ),
            })
            .collect()
    }

    /// Returns a vec of transparent outputs: (value, script_hash).
    pub fn transparent_outputs(&self) -> Vec<(u64, Vec<u8>)> {
        self.transaction
            .outputs()
            .iter()
            .filter_map(|output| {
                u64::try_from(output.value().zatoshis())
                    .ok()
                    .map(|value| (value, output.lock_script.as_raw_bytes().to_vec()))
            })
            .collect()
    }

    /// Returns sapling, orchard, and ironwood value balances for the transaction.
    ///
    /// Returned as (Option\<valueBalanceSapling\>, Option\<valueBalanceOrchard\>,
    /// Option\<valueBalanceIronwood\>).
    pub fn value_balances(&self) -> (Option<i64>, Option<i64>, Option<i64>) {
        let sapling = if self.version() == 4 || self.transaction.has_sapling_shielded_data() {
            Some(
                self.transaction
                    .sapling_value_balance()
                    .sapling_amount()
                    .zatoshis(),
            )
        } else {
            None
        };

        let orchard = self.transaction.has_orchard_shielded_data().then(|| {
            self.transaction
                .orchard_value_balance()
                .orchard_amount()
                .zatoshis()
        });

        let ironwood = self.transaction.has_ironwood_shielded_data().then(|| {
            self.transaction
                .ironwood_value_balance()
                .ironwood_amount()
                .zatoshis()
        });

        (sapling, orchard, ironwood)
    }

    /// Returns a vec of sapling nullifiers for the transaction.
    pub fn shielded_spends(&self) -> Vec<Vec<u8>> {
        self.transaction
            .sapling_nullifiers()
            .map(|nullifier| <[u8; 32]>::from(*nullifier).to_vec())
            .collect()
    }

    /// Returns a vec of sapling outputs (cmu, ephemeral_key, enc_ciphertext) for the transaction.
    pub fn shielded_outputs(&self) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        self.transaction
            .sapling_outputs()
            .map(|output| {
                let ephemeral_key: [u8; 32] = (&output.ephemeral_key).into();
                let enc_ciphertext: [u8; 580] = output.enc_ciphertext.into();

                (
                    output.cm_u.to_bytes().to_vec(),
                    ephemeral_key.to_vec(),
                    enc_ciphertext.to_vec(),
                )
            })
            .collect()
    }

    /// Returns None as joinsplits are not supported in Zaino.
    pub fn join_splits(&self) -> Option<()> {
        None
    }

    /// Returns a vec of orchard actions (nullifier, cmx, ephemeral_key, enc_ciphertext) for the transaction.
    #[allow(clippy::complexity)]
    pub fn orchard_actions(&self) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
        self.transaction
            .orchard_actions()
            .map(|action| {
                let nullifier: [u8; 32] = action.nullifier.into();
                let cmx: [u8; 32] = action.cm_x.into();
                let ephemeral_key: [u8; 32] = (&action.ephemeral_key).into();
                let enc_ciphertext: [u8; 580] = action.enc_ciphertext.into();

                (
                    nullifier.to_vec(),
                    cmx.to_vec(),
                    ephemeral_key.to_vec(),
                    enc_ciphertext.to_vec(),
                )
            })
            .collect()
    }

    /// Returns a vec of ironwood actions (nullifier, cmx, ephemeral_key, enc_ciphertext) for the transaction.
    #[allow(clippy::complexity)]
    pub fn ironwood_actions(&self) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
        self.transaction
            .ironwood_actions()
            .map(|action| {
                let nullifier: [u8; 32] = action.nullifier.into();
                let cmx: [u8; 32] = action.cm_x.into();
                let ephemeral_key: [u8; 32] = (&action.ephemeral_key).into();
                let enc_ciphertext: [u8; 580] = action.enc_ciphertext.into();

                (
                    nullifier.to_vec(),
                    cmx.to_vec(),
                    ephemeral_key.to_vec(),
                    enc_ciphertext.to_vec(),
                )
            })
            .collect()
    }

    /// Returns the orchard anchor of the transaction.
    ///
    /// If this is the Coinbase transaction then this returns the AuthDataRoot of the block.
    pub fn anchor_orchard(&self) -> Option<Vec<u8>> {
        self.transaction
            .orchard_shielded_data()
            .map(|shielded_data| <[u8; 32]>::from(&shielded_data.shared_anchor).to_vec())
    }

    /// Returns the transaction as raw bytes.
    pub fn raw_bytes(&self) -> Vec<u8> {
        self.raw_bytes.clone()
    }

    /// Returns the TxId of the transaction.
    pub fn tx_id(&self) -> Vec<u8> {
        self.tx_id.clone()
    }

    /// Converts a zcash full transaction into a compact transaction.
    #[deprecated]
    pub fn to_compact(self, index: u64) -> Result<CompactTx, ParseError> {
        self.to_compact_tx(Some(index), &PoolTypeFilter::default())
    }

    /// Converts a Zcash Transaction into a `CompactTx` of the Light wallet protocol.
    /// If the transaction you want to convert is a mempool transaction you can specify `None`.
    /// Specify the `PoolType`s that the transaction should include in the `pool_types` argument
    /// with a `PoolTypeFilter` indicating which pools the compact block should include.
    pub fn to_compact_tx(
        self,
        index: Option<u64>,
        pool_types: &PoolTypeFilter,
    ) -> Result<CompactTx, ParseError> {
        let spends = if pool_types.includes_sapling() {
            self.shielded_spends()
                .into_iter()
                .map(|nf| CompactSaplingSpend { nf })
                .collect()
        } else {
            Vec::new()
        };

        let outputs = if pool_types.includes_sapling() {
            self.shielded_outputs()
                .into_iter()
                .map(
                    |(cmu, ephemeral_key, enc_ciphertext)| CompactSaplingOutput {
                        cmu,
                        ephemeral_key,
                        ciphertext: enc_ciphertext[..52].to_vec(),
                    },
                )
                .collect()
        } else {
            Vec::new()
        };

        let actions = if pool_types.includes_orchard() {
            self.orchard_actions()
                .into_iter()
                .map(
                    |(nullifier, cmx, ephemeral_key, enc_ciphertext)| CompactOrchardAction {
                        nullifier,
                        cmx,
                        ephemeral_key,
                        ciphertext: enc_ciphertext[..52].to_vec(),
                    },
                )
                .collect()
        } else {
            Vec::new()
        };

        let ironwood_actions = if pool_types.includes_ironwood() {
            self.ironwood_actions()
                .into_iter()
                .map(
                    |(nullifier, cmx, ephemeral_key, enc_ciphertext)| CompactOrchardAction {
                        nullifier,
                        cmx,
                        ephemeral_key,
                        ciphertext: enc_ciphertext[..52].to_vec(),
                    },
                )
                .collect()
        } else {
            Vec::new()
        };

        let vout = if pool_types.includes_transparent() {
            self.transparent_outputs()
                .into_iter()
                .map(|(value, script_hash)| CompactTxOut {
                    value,
                    script_pub_key: script_hash,
                })
                .collect()
        } else {
            Vec::new()
        };

        let vin = if pool_types.includes_transparent() {
            self.transaction
                .inputs()
                .iter()
                .filter_map(|input| {
                    let outpoint = input.outpoint()?;
                    Some(CompactTxIn {
                        prevout_txid: outpoint.hash.0.to_vec(),
                        prevout_index: outpoint.index,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        Ok(CompactTx {
            index: index.unwrap_or(0), // this assumes that mempool txs have a zeroed index
            txid: self.tx_id(),
            fee: 0,
            spends,
            outputs,
            actions,
            ironwood_actions,
            vin,
            vout,
        })
    }

    /// Returns true if the transaction contains either sapling spends or outputs, or orchard actions.
    #[allow(dead_code)]
    pub(crate) fn has_shielded_elements(&self) -> bool {
        self.transaction.has_sapling_shielded_data()
            || self.transaction.has_orchard_shielded_data()
            || self.transaction.has_ironwood_shielded_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire_serialized_transaction_test_data::transactions::get_test_vectors;

    #[test]
    fn parses_test_vectors_with_zebra_deserializer() -> Result<(), ParseError> {
        for vector in get_test_vectors() {
            let (remaining, transaction) = FullTransaction::parse_from_slice(
                &vector.tx,
                Some(vec![vector.txid.to_vec()]),
                None,
            )?;

            assert!(remaining.is_empty(), "{}", vector.description);
            assert_eq!(
                transaction.version(),
                vector.version,
                "{}",
                vector.description
            );
            assert_eq!(
                transaction.transparent_inputs().len(),
                vector.transparent_inputs,
                "{}",
                vector.description
            );
            assert_eq!(
                transaction.transparent_outputs().len(),
                vector.transparent_outputs,
                "{}",
                vector.description
            );
        }

        Ok(())
    }
}
