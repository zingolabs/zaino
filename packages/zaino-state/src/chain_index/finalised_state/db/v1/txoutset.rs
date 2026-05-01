//! Native transparent UTXO-set index helpers for `gettxoutsetinfo`.

use super::*;

const TXOUTSET_META_KEY: &[u8] = b"txoutset_meta";
const TXOUTSET_NOT_STARTED_HEIGHT: u32 = u32::MAX;

impl DbV1 {
    /// Applies a full Zebra block to the native txoutset tables inside an existing write transaction.
    ///
    /// This path intentionally consumes the full block, not Zaino's compact transparent records,
    /// because zcashd-compatible `hash_serialized` requires exact `scriptPubKey` bytes.
    pub(crate) fn apply_txoutset_block_blocking(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        block: &zebra_chain::block::Block,
    ) -> Result<(), FinalisedStateError> {
        let height = block.coinbase_height().ok_or_else(|| {
            FinalisedStateError::Custom(
                "cannot apply txoutset block without coinbase height".into(),
            )
        })?;
        let height = height.0;
        let block_hash = BlockHash::from(block.hash().0);
        let mut meta = self.read_txoutset_meta_from_txn(txn)?;

        if meta.built_to_height() == TXOUTSET_NOT_STARTED_HEIGHT {
            if height != GENESIS_HEIGHT.0 {
                return Err(FinalisedStateError::Custom(format!(
                    "empty txoutset index must start at genesis, got height {height}"
                )));
            }
        } else {
            let expected_next = meta.built_to_height().checked_add(1).ok_or_else(|| {
                FinalisedStateError::Custom("txoutset migration height overflow".into())
            })?;
            if height != expected_next {
                return Err(FinalisedStateError::Custom(format!(
                    "txoutset block application must be contiguous: got {height}, expected {expected_next}"
                )));
            }
        }

        let mut txouts = meta.txouts();
        let mut total_amount_zat = meta.total_amount_zat();

        for (tx_index, transaction) in block.transactions.iter().enumerate() {
            for input in transaction.inputs() {
                if let Some(outpoint) = input.outpoint() {
                    let zaino_outpoint = Outpoint::new(outpoint.hash.0, outpoint.index);
                    let outpoint_key = zaino_outpoint.to_bytes()?;
                    let spent_entry_bytes = txn.get(self.txoutset_utxos, &outpoint_key)?;
                    let spent_entry = StoredEntryVar::<TxOutSetUtxo>::from_bytes(spent_entry_bytes)
                        .map_err(|e| {
                            FinalisedStateError::Custom(format!("corrupt txoutset UTXO entry: {e}"))
                        })?;

                    if !spent_entry.verify(&outpoint_key) {
                        return Err(FinalisedStateError::Custom(
                            "txoutset UTXO checksum mismatch".into(),
                        ));
                    }

                    txn.del(self.txoutset_utxos, &outpoint_key, None)?;
                    self.decrement_txoutset_tx_count(txn, TransactionHash(outpoint.hash.0))?;
                    txouts = txouts.checked_sub(1).ok_or_else(|| {
                        FinalisedStateError::Custom("txoutset txout counter underflow".into())
                    })?;
                    total_amount_zat = total_amount_zat
                        .checked_sub(spent_entry.inner().value_zat())
                        .ok_or_else(|| {
                            FinalisedStateError::Custom(
                                "txoutset total amount counter underflow".into(),
                            )
                        })?;
                }
            }

            let txid = TransactionHash::from(transaction.hash());
            let mut unspent_outputs_for_tx = 0u32;

            for (output_index, output) in transaction.outputs().iter().enumerate() {
                let output_index = u32::try_from(output_index).map_err(|_| {
                    FinalisedStateError::Custom("transparent output index overflow".into())
                })?;
                let outpoint = Outpoint::new(txid.0, output_index);
                let outpoint_key = outpoint.to_bytes()?;
                let value_zat = u64::from(output.value);
                let script_pubkey = output.lock_script.as_raw_bytes().to_vec();
                let utxo = TxOutSetUtxo::new(
                    value_zat,
                    script_pubkey,
                    height,
                    transaction.is_coinbase(),
                    u32::try_from(tx_index).map_err(|_| {
                        FinalisedStateError::Custom("transaction index overflow".into())
                    })?,
                );
                let utxo_entry = StoredEntryVar::new(&outpoint_key, utxo);

                txn.put(
                    self.txoutset_utxos,
                    &outpoint_key,
                    &utxo_entry.to_bytes()?,
                    WriteFlags::NO_OVERWRITE,
                )?;

                unspent_outputs_for_tx =
                    unspent_outputs_for_tx.checked_add(1).ok_or_else(|| {
                        FinalisedStateError::Custom(
                            "txoutset per-transaction count overflow".into(),
                        )
                    })?;
                txouts = txouts.checked_add(1).ok_or_else(|| {
                    FinalisedStateError::Custom("txoutset txout counter overflow".into())
                })?;
                total_amount_zat = total_amount_zat.checked_add(value_zat).ok_or_else(|| {
                    FinalisedStateError::Custom("txoutset total amount counter overflow".into())
                })?;
            }

            if unspent_outputs_for_tx > 0 {
                self.increment_txoutset_tx_count(txn, txid, unspent_outputs_for_tx)?;
            }
        }

        meta = TxOutSetMeta::new(height, block_hash.0, false, txouts, total_amount_zat);
        self.write_txoutset_meta_to_txn(txn, meta)
    }

    pub(crate) fn read_txoutset_meta_from_txn<T: lmdb::Transaction>(
        &self,
        txn: &T,
    ) -> Result<TxOutSetMeta, FinalisedStateError> {
        match txn.get(self.txoutset_meta, &TXOUTSET_META_KEY) {
            Ok(bytes) => {
                let entry = StoredEntryFixed::<TxOutSetMeta>::from_bytes(bytes).map_err(|e| {
                    FinalisedStateError::Custom(format!("corrupt txoutset metadata entry: {e}"))
                })?;
                if !entry.verify(TXOUTSET_META_KEY) {
                    return Err(FinalisedStateError::Custom(
                        "txoutset metadata checksum mismatch".into(),
                    ));
                }
                Ok(*entry.inner())
            }
            Err(lmdb::Error::NotFound) => Ok(TxOutSetMeta::new(
                TXOUTSET_NOT_STARTED_HEIGHT,
                [0u8; 32],
                false,
                0,
                0,
            )),
            Err(e) => Err(FinalisedStateError::LmdbError(e)),
        }
    }

    pub(crate) fn write_txoutset_meta_to_txn(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        meta: TxOutSetMeta,
    ) -> Result<(), FinalisedStateError> {
        let entry = StoredEntryFixed::new(TXOUTSET_META_KEY, meta);
        txn.put(
            self.txoutset_meta,
            &TXOUTSET_META_KEY,
            &entry.to_bytes()?,
            WriteFlags::empty(),
        )?;
        Ok(())
    }

    fn increment_txoutset_tx_count(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        txid: TransactionHash,
        by: u32,
    ) -> Result<(), FinalisedStateError> {
        let txid_key = txid.to_bytes()?;
        let current = match txn.get(self.txoutset_tx_counts, &txid_key) {
            Ok(bytes) => {
                let entry =
                    StoredEntryFixed::<TxOutSetTxCount>::from_bytes(bytes).map_err(|e| {
                        FinalisedStateError::Custom(format!("corrupt txoutset tx count entry: {e}"))
                    })?;
                if !entry.verify(&txid_key) {
                    return Err(FinalisedStateError::Custom(
                        "txoutset tx count checksum mismatch".into(),
                    ));
                }
                entry.inner().count()
            }
            Err(lmdb::Error::NotFound) => 0,
            Err(e) => return Err(FinalisedStateError::LmdbError(e)),
        };
        let updated = current.checked_add(by).ok_or_else(|| {
            FinalisedStateError::Custom("txoutset per-transaction count overflow".into())
        })?;
        let entry = StoredEntryFixed::new(&txid_key, TxOutSetTxCount::new(updated));
        txn.put(
            self.txoutset_tx_counts,
            &txid_key,
            &entry.to_bytes()?,
            WriteFlags::empty(),
        )?;
        Ok(())
    }

    fn decrement_txoutset_tx_count(
        &self,
        txn: &mut lmdb::RwTransaction<'_>,
        txid: TransactionHash,
    ) -> Result<(), FinalisedStateError> {
        let txid_key = txid.to_bytes()?;
        let bytes = txn.get(self.txoutset_tx_counts, &txid_key)?;
        let entry = StoredEntryFixed::<TxOutSetTxCount>::from_bytes(bytes).map_err(|e| {
            FinalisedStateError::Custom(format!("corrupt txoutset tx count entry: {e}"))
        })?;
        if !entry.verify(&txid_key) {
            return Err(FinalisedStateError::Custom(
                "txoutset tx count checksum mismatch".into(),
            ));
        }

        match entry.inner().count().checked_sub(1) {
            Some(0) => {
                txn.del(self.txoutset_tx_counts, &txid_key, None)?;
            }
            Some(updated) => {
                let updated_entry = StoredEntryFixed::new(&txid_key, TxOutSetTxCount::new(updated));
                txn.put(
                    self.txoutset_tx_counts,
                    &txid_key,
                    &updated_entry.to_bytes()?,
                    WriteFlags::empty(),
                )?;
            }
            None => {
                return Err(FinalisedStateError::Custom(
                    "txoutset per-transaction count underflow".into(),
                ));
            }
        }

        Ok(())
    }
}
