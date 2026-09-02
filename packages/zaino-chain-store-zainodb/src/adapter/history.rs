//! Transparent address history.
//!
//! Feature-gated and experimental: the port exists, the capability bit gates
//! it, and a build without the feature does not implement it at all. Kept apart
//! from the other port impls so that gate is one `mod` declaration rather than
//! a `cfg` on each method.

use super::error_map::chain_store_error;
use super::from_domain::stored_script_tag;
use super::to_domain::{block_tx_position, domain_txid, stored_tx_out};
use zaino_chain_store::{ChainStoreError, ChainStoreSource, StoredAddress};
use zaino_primitives::types::{Outpoint as DomainOutpoint, TransactionId};

use crate::error::StoreError;
use crate::store::reader::DbReader;
use crate::types::{Outpoint, TxOutCompact};

/// What the finalised range shows happening to a set of addresses.
///
/// Assembled from the address index plus the transparent rows, rather than
/// served from a single table: the index records *which transactions touched an
/// address*, and the domain answer needs what those transactions did, which
/// means reading the transaction. That is also why spends carry the output they
/// spent — resolving each input's previous output is work the store has to do
/// anyway to decide whether the input belongs to the address, so handing the
/// result over costs nothing and saves the consumer a second round trip.
#[cfg(feature = "transparent_address_history_experimental")]
impl<T: ChainStoreSource> zaino_chain_store::TransparentHistoryIndex for DbReader<T> {
    async fn address_effects(
        &self,
        query: &zaino_chain_store::TransparentHistoryQuery,
    ) -> Result<zaino_chain_store::StoreAddressEffects, ChainStoreError> {
        use zaino_chain_store::{LocatedOutput, LocatedSpend, StoreAddressEffects};

        if query.start > query.end {
            return Err(ChainStoreError::InvalidRange {
                start: query.start,
                end: query.end,
            });
        }
        let Some((start, end)) = self.clamped_range(query.start, query.end)? else {
            return Ok(StoreAddressEffects::default());
        };

        let mut effects = StoreAddressEffects::default();

        for address in &query.addresses {
            let key =
                crate::types::AddrScript::new(address.hash, stored_script_tag(address.script_type));
            let Some(locations) = self
                .addr_tx_locations_by_range(key, start, end)
                .await
                .map_err(chain_store_error)?
            else {
                continue;
            };

            for location in locations {
                let position = block_tx_position(location)?;
                let txid = domain_txid(self.get_txid(location).await.map_err(chain_store_error)?);
                let Some(transparent) = DbReader::get_transparent(self, location)
                    .await
                    .map_err(chain_store_error)?
                else {
                    continue;
                };

                // Outputs this transaction created that pay the address.
                for (index, output) in transparent.outputs().iter().enumerate() {
                    if !pays(output, address) {
                        continue;
                    }
                    effects.outputs.push(LocatedOutput {
                        outpoint: DomainOutpoint {
                            txid,
                            index: index as u32,
                        },
                        output: stored_tx_out(output)?,
                        position,
                        txid,
                    });
                }

                // Inputs this transaction spent that belonged to the address.
                //
                // The coinbase's null prevout spends nothing, and
                // `spent_outpoints` already drops it — the same filter the
                // store's own spend index applies.
                for outpoint in transparent.spent_outpoints() {
                    let Some(previous) = self.previous_output_row(outpoint).await? else {
                        continue;
                    };
                    if !pays(&previous, address) {
                        continue;
                    }
                    effects.spends.push(LocatedSpend {
                        outpoint: DomainOutpoint {
                            txid: TransactionId::from(*outpoint.prev_txid()),
                            index: outpoint.prev_index(),
                        },
                        output: stored_tx_out(&previous)?,
                        position,
                        txid,
                    });
                }
            }
        }

        Ok(effects)
    }
}

/// Whether a stored output is keyed under `address`.
#[cfg(feature = "transparent_address_history_experimental")]
fn pays(output: &TxOutCompact, address: &StoredAddress) -> bool {
    *output.script_hash() == address.hash
        && output.script_type() == stored_script_tag(address.script_type)
}

#[cfg(feature = "transparent_address_history_experimental")]
impl<T: ChainStoreSource> DbReader<T> {
    /// The stored output an outpoint names, with a miss as `None`.
    async fn previous_output_row(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxOutCompact>, ChainStoreError> {
        match DbReader::get_previous_output(self, outpoint).await {
            Ok(output) => Ok(Some(output)),
            Err(StoreError::DataUnavailable(_)) => Ok(None),
            Err(error) => Err(chain_store_error(error)),
        }
    }
}
