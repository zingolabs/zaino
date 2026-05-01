//! Native transparent UTXO-set index helpers for `gettxoutsetinfo`.

use super::*;

const TXOUTSET_META_KEY: &[u8] = b"txoutset_meta";
const TXOUTSET_NOT_STARTED_HEIGHT: u32 = u32::MAX;

impl DbV1 {
    /// Applies a full Zebra block to the native txoutset tables and commits it atomically.
    pub(crate) async fn apply_txoutset_block(
        &self,
        block: Arc<zebra_chain::block::Block>,
    ) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;
            self.apply_txoutset_block_blocking(&mut txn, block.as_ref())?;
            txn.commit()?;
            Ok(())
        })
    }

    /// Returns the highest height already applied to the txoutset index, if any.
    pub(crate) async fn txoutset_built_to_height(
        &self,
    ) -> Result<Option<Height>, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let meta = self.read_txoutset_meta_from_txn(&txn)?;
            if meta.built_to_height() == TXOUTSET_NOT_STARTED_HEIGHT {
                Ok(None)
            } else {
                Ok(Some(Height(meta.built_to_height())))
            }
        })
    }

    /// Verifies the txoutset tables and marks migration complete.
    pub(crate) async fn finalize_txoutset_migration(
        &self,
        expected_height: Height,
        expected_hash: BlockHash,
    ) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;
            let meta = self.read_txoutset_meta_from_txn(&txn)?;

            if meta.built_to_height() != expected_height.0 {
                return Err(FinalisedStateError::Custom(format!(
                    "txoutset migration sanity check failed: built height {}, expected {}",
                    meta.built_to_height(),
                    expected_height.0
                )));
            }
            if meta.best_block_hash() != &expected_hash.0 {
                return Err(FinalisedStateError::Custom(
                    "txoutset migration sanity check failed: best block hash mismatch".into(),
                ));
            }

            let mut txouts = 0u64;
            let mut total_amount_zat = 0u64;
            {
                let mut utxo_cursor = txn.open_ro_cursor(self.txoutset_utxos)?;
                for (outpoint_key, utxo_bytes) in utxo_cursor.iter() {
                    let entry =
                        StoredEntryVar::<TxOutSetUtxo>::from_bytes(utxo_bytes).map_err(|e| {
                            FinalisedStateError::Custom(format!("corrupt txoutset UTXO entry: {e}"))
                        })?;
                    if !entry.verify(outpoint_key) {
                        return Err(FinalisedStateError::Custom(
                            "txoutset migration sanity check failed: UTXO checksum mismatch".into(),
                        ));
                    }
                    txouts = txouts.checked_add(1).ok_or_else(|| {
                        FinalisedStateError::Custom("txoutset txout counter overflow".into())
                    })?;
                    total_amount_zat = total_amount_zat
                        .checked_add(entry.inner().value_zat())
                        .ok_or_else(|| {
                            FinalisedStateError::Custom(
                                "txoutset total amount counter overflow".into(),
                            )
                        })?;
                }
            }

            if txouts != meta.txouts() || total_amount_zat != meta.total_amount_zat() {
                return Err(FinalisedStateError::Custom(format!(
                    "txoutset migration sanity check failed: aggregate mismatch \
                     (txouts {txouts}/{}, total {total_amount_zat}/{})",
                    meta.txouts(),
                    meta.total_amount_zat()
                )));
            }

            let mut txout_count_sum = 0u64;
            {
                let mut tx_count_cursor = txn.open_ro_cursor(self.txoutset_tx_counts)?;
                for (txid_key, count_bytes) in tx_count_cursor.iter() {
                    let entry = StoredEntryFixed::<TxOutSetTxCount>::from_bytes(count_bytes)
                        .map_err(|e| {
                            FinalisedStateError::Custom(format!(
                                "corrupt txoutset tx count entry: {e}"
                            ))
                        })?;
                    if !entry.verify(txid_key) {
                        return Err(FinalisedStateError::Custom(
                            "txoutset migration sanity check failed: tx count checksum mismatch"
                                .into(),
                        ));
                    }
                    txout_count_sum = txout_count_sum
                        .checked_add(u64::from(entry.inner().count()))
                        .ok_or_else(|| {
                            FinalisedStateError::Custom(
                                "txoutset tx count aggregate overflow".into(),
                            )
                        })?;
                }
            }

            if txout_count_sum != txouts {
                return Err(FinalisedStateError::Custom(format!(
                    "txoutset migration sanity check failed: tx-count sum {txout_count_sum}, txouts {txouts}"
                )));
            }

            let complete_meta = TxOutSetMeta::new(
                meta.built_to_height(),
                *meta.best_block_hash(),
                true,
                meta.txouts(),
                meta.total_amount_zat(),
            );
            self.write_txoutset_meta_to_txn(&mut txn, complete_meta)?;
            txn.commit()?;
            Ok(())
        })
    }

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

        meta = TxOutSetMeta::new(
            height,
            block_hash.0,
            meta.migration_complete(),
            txouts,
            total_amount_zat,
        );
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
