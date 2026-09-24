use super::*;

impl DbV1 {
    /// Checks that the spent index, and the address history when compiled in, agree with the transparent data stored at `height`.
    pub(super) fn check_block_indexes_blocking(&self, height: Height) -> Result<(), StoreError> {
        let height_key = height.to_bytes()?;
        let ro = self.env.begin_ro_txn()?;
        let header =
            BlockHeaderData::<AbsoluteChainWork>::from_bytes(ro.get(self.headers, &height_key)?)?;
        let fail = |reason: String| StoreError::InvalidBlock {
            height: height.0,
            hash: header.context.index.hash,
            reason,
        };
        let transparent = TransparentTxList::from_bytes(ro.get(self.transparent, &height_key)?)?;

        for (tx_index, tx) in transparent.tx().iter().enumerate() {
            let Some(tx) = tx else { continue };
            let tx_index = u16::try_from(tx_index)
                .map_err(|_| fail(format!("transaction index {tx_index} out of range")))?;
            let tx_location = TxLocation::new(height.0, tx_index);

            for outpoint in tx.spent_outpoints() {
                let stored = match ro.get(self.spent, &outpoint.to_bytes()?) {
                    Ok(bytes) => TxLocation::from_bytes(bytes)?,
                    Err(lmdb::Error::NotFound) => {
                        return Err(fail(format!(
                            "missing spent index for outpoint {outpoint:?}"
                        )))
                    }
                    Err(error) => return Err(StoreError::LmdbError(error)),
                };
                if stored != tx_location {
                    return Err(fail(format!(
                        "spent index for outpoint {outpoint:?} names {stored:?}, not {tx_location:?}"
                    )));
                }
            }

            #[cfg(feature = "transparent_address_history_experimental")]
            self.check_address_history_in_txn(&ro, tx, tx_location, &fail)?;
        }
        Ok(())
    }

    /// Checks that every output of `tx` has a mined record, and every non-coinbase input a spending record, in the address history.
    #[cfg(feature = "transparent_address_history_experimental")]
    fn check_address_history_in_txn(
        &self,
        ro: &lmdb::RoTransaction<'_>,
        tx: &TransparentCompactTx,
        tx_location: TxLocation,
        fail: &impl Fn(String) -> StoreError,
    ) -> Result<(), StoreError> {
        for (vout, output) in tx.outputs().iter().enumerate() {
            let addr_bytes =
                AddrScript::new(*output.script_hash(), output.script_type()).to_bytes()?;
            let records = self.addr_hist_records_in_txn(ro, &addr_bytes, tx_location)?;
            if !records
                .iter()
                .any(|record| record.is_mined() && usize::from(record.out_index()) == vout)
            {
                return Err(fail(format!(
                    "missing address-history mined record for output {vout} of {tx_location:?}"
                )));
            }
        }

        for (input_index, input) in tx.inputs().iter().enumerate() {
            if input.is_null_prevout() {
                continue;
            }
            let outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
            let prev_output = self.get_previous_output_blocking(outpoint)?;
            let addr_bytes = AddrScript::new(*prev_output.script_hash(), prev_output.script_type())
                .to_bytes()?;
            let records = self.addr_hist_records_in_txn(ro, &addr_bytes, tx_location)?;
            if !records
                .iter()
                .any(|record| record.is_input() && usize::from(record.out_index()) == input_index)
            {
                return Err(fail(format!(
                    "missing address-history input record for input {input_index} of {tx_location:?}"
                )));
            }
        }
        Ok(())
    }

    /// Returns the decoded address-history records of `addr_bytes` at `tx_location`.
    #[cfg(feature = "transparent_address_history_experimental")]
    fn addr_hist_records_in_txn(
        &self,
        ro: &lmdb::RoTransaction<'_>,
        addr_bytes: &[u8],
        tx_location: TxLocation,
    ) -> Result<Vec<AddrHistRecord>, StoreError> {
        self.addr_hist_records_by_addr_and_index_in_txn(ro, addr_bytes, tx_location)?
            .iter()
            .map(|bytes| Ok(AddrEventBytes::from_bytes(bytes)?.as_record()?))
            .collect()
    }
}

#[cfg(test)]
mod check_indexes_to_tip {
    use super::*;
    use crate::tests::fixtures::{load_test_vectors, sync_db_with_blockdata};
    use tempfile::TempDir;
    use zaino_chain_store::ChainStoreConfig;
    use zaino_common::network::ActivationHeights;

    /// A persistent regtest store holding every vector block, its directory, and its tip.
    async fn synced_store() -> (TempDir, DbV1, Height) {
        let temp_dir = tempfile::tempdir().expect("a temporary directory is created");
        let config = StoreSettings::new(
            ChainStoreConfig::at_path(temp_dir.path().to_path_buf()),
            crate::config::ZainoDbConfig::new(ActivationHeights::default().to_regtest_network()),
        );
        let db = DbV1::spawn(&config).await.expect("a fresh database opens");
        let blocks = load_test_vectors().expect("the vectors load").blocks;
        sync_db_with_blockdata(&db, &blocks, None).await;
        let tip = db
            .tip_height()
            .await
            .expect("the tip reads")
            .expect("the store holds blocks");
        (temp_dir, db, tip)
    }

    /// The first stored block that spends a transparent output, and one outpoint it spends.
    fn first_spend(db: &DbV1, tip: Height) -> (Height, Outpoint) {
        let ro = db.env.begin_ro_txn().expect("a read transaction opens");
        (GENESIS_HEIGHT.0..=tip.0)
            .map(Height)
            .find_map(|height| {
                let key = height.to_bytes().expect("a height encodes");
                let row = ro
                    .get(db.transparent, &key)
                    .expect("the transparent row reads");
                TransparentTxList::from_bytes(row)
                    .expect("the transparent row decodes")
                    .tx()
                    .iter()
                    .flatten()
                    .find_map(|tx| tx.spent_outpoints().next())
                    .map(|outpoint| (height, outpoint))
            })
            .expect("the vectors spend a transparent output")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn passes_every_block_the_write_path_stores() {
        let (_temp_dir, db, tip) = synced_store().await;
        let mut next = GENESIS_HEIGHT;

        db.check_indexes_to_tip(&mut next)
            .await
            .expect("the write path keeps its indexes consistent");

        assert_eq!(next, Height(tip.0 + 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stops_at_the_block_whose_spent_row_is_missing() {
        let (_temp_dir, db, tip) = synced_store().await;
        let (spending_height, outpoint) = first_spend(&db, tip);
        let mut txn = db.env.begin_rw_txn().expect("a write transaction opens");
        txn.del(
            db.spent,
            &outpoint.to_bytes().expect("an outpoint encodes"),
            None,
        )
        .expect("the spent row deletes");
        txn.commit().expect("the delete commits");
        let mut next = GENESIS_HEIGHT;

        let result = db.check_indexes_to_tip(&mut next).await;

        assert!(
            matches!(result, Err(StoreError::InvalidBlock { height, .. }) if height == spending_height.0),
            "expected InvalidBlock at {spending_height:?}, got {result:?}"
        );
        assert_eq!(next, spending_height);
    }
}
