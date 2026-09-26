//! The write loop derives the spent index, and the address history when compiled in, from each
//! block it stores; these tests hold that derivation to the blocks themselves.

use std::collections::HashMap;

use lmdb::Transaction as _;
use tempfile::TempDir;
use zaino_chain_store::ChainStoreConfig;
use zaino_common::network::ActivationHeights;

use super::*;
use crate::tests::fixtures::{indexed_block_chain, load_test_vectors};

/// A fresh persistent regtest store in its own temporary directory.
async fn empty_store() -> (TempDir, DbV1) {
    let temp_dir = tempfile::tempdir().expect("a temporary directory is created");
    let config = StoreSettings::new(
        ChainStoreConfig::at_path(temp_dir.path().to_path_buf()),
        crate::config::ZainoDbConfig::new(ActivationHeights::default().to_regtest_network()),
    );
    let db = DbV1::spawn(&config).await.expect("a fresh database opens");
    (temp_dir, db)
}

/// The vector chain as the indexed blocks the write path takes.
fn vector_chain() -> Vec<IndexedBlock<AbsoluteChainWork>> {
    let vectors = load_test_vectors().expect("the vectors load");
    indexed_block_chain(&vectors.blocks).collect()
}

/// Every outpoint the given blocks spend, mapped to the location of the transaction spending it.
fn expected_spent_index(
    blocks: &[IndexedBlock<AbsoluteChainWork>],
) -> HashMap<Outpoint, TxLocation> {
    let mut expected = HashMap::new();
    for block in blocks {
        let height = block.context.index.height.0;
        for (tx_index, tx) in block.transactions().iter().enumerate() {
            let location = TxLocation::new(height, tx_index as u16);
            for outpoint in tx.transparent().spent_outpoints() {
                assert!(
                    expected.insert(outpoint, location).is_none(),
                    "the vector chain spends {outpoint:?} twice"
                );
            }
        }
    }
    expected
}

/// Asserts that the stored spent index is exactly `expected`, row for row.
fn assert_spent_index_is(db: &DbV1, expected: &HashMap<Outpoint, TxLocation>) {
    let ro = db.env.begin_ro_txn().expect("a read transaction opens");
    for (outpoint, location) in expected {
        let key = outpoint.to_bytes().expect("an outpoint encodes");
        let stored = ro.get(db.spent, &key).expect("the spent row exists");
        let stored = TxLocation::from_bytes(stored).expect("the spent row decodes");
        assert_eq!(&stored, location, "spent row for {outpoint:?}");
    }
    let rows = ro
        .open_ro_cursor(db.spent)
        .expect("a cursor opens on the spent table")
        .iter_start()
        .count();
    assert_eq!(rows, expected.len(), "spent rows beyond the expected ones");
}

/// Asserts that every output of every transaction in `blocks` has a mined record, and every
/// non-coinbase input a spending record, in the address history.
#[cfg(feature = "transparent_address_history_experimental")]
fn assert_address_history_covers(db: &DbV1, blocks: &[IndexedBlock<AbsoluteChainWork>]) {
    let mut outputs_by_outpoint = HashMap::new();
    for block in blocks {
        for tx in block.transactions() {
            for (vout, output) in tx.transparent().outputs().iter().enumerate() {
                outputs_by_outpoint.insert(Outpoint::new(tx.txid().0, vout as u32), *output);
            }
        }
    }

    let ro = db.env.begin_ro_txn().expect("a read transaction opens");
    let records_at = |addr_bytes: &[u8], location: TxLocation| -> Vec<AddrHistRecord> {
        db.addr_hist_records_by_addr_and_index_in_txn(&ro, addr_bytes, location)
            .expect("the address-history rows read")
            .iter()
            .map(|bytes| {
                AddrEventBytes::from_bytes(bytes)
                    .expect("an address-history row decodes")
                    .as_record()
                    .expect("an address-history row is a record")
            })
            .collect()
    };

    for block in blocks {
        let height = block.context.index.height.0;
        for (tx_index, tx) in block.transactions().iter().enumerate() {
            let location = TxLocation::new(height, tx_index as u16);

            for (vout, output) in tx.transparent().outputs().iter().enumerate() {
                let addr_bytes = AddrScript::new(*output.script_hash(), output.script_type())
                    .to_bytes()
                    .expect("an address script encodes");
                assert!(
                    records_at(&addr_bytes, location)
                        .iter()
                        .any(|record| record.is_mined() && usize::from(record.out_index()) == vout),
                    "missing mined record for output {vout} of {location:?}"
                );
            }

            for (input_index, input) in tx.transparent().inputs().iter().enumerate() {
                if input.is_null_prevout() {
                    continue;
                }
                let outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                let previous = outputs_by_outpoint
                    .get(&outpoint)
                    .expect("the vector chain spends an output it created");
                let addr_bytes = AddrScript::new(*previous.script_hash(), previous.script_type())
                    .to_bytes()
                    .expect("an address script encodes");
                assert!(
                    records_at(&addr_bytes, location)
                        .iter()
                        .any(|record| record.is_input()
                            && usize::from(record.out_index()) == input_index),
                    "missing input record for input {input_index} of {location:?}"
                );
            }
        }
    }
}

// multi_thread required: the write path runs LMDB work through `block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn the_single_block_write_indexes_every_spend_at_its_location() {
    let (_temp_dir, db) = empty_store().await;
    let blocks = vector_chain();

    for block in blocks.iter().cloned() {
        db.write_block(block).await.expect("a vector block writes");
    }

    assert_spent_index_is(&db, &expected_spent_index(&blocks));
    #[cfg(feature = "transparent_address_history_experimental")]
    assert_address_history_covers(&db, &blocks);
}

// multi_thread required: the batch write runs LMDB work through `block_in_place`.
#[cfg(not(feature = "transparent_address_history_experimental"))]
#[tokio::test(flavor = "multi_thread")]
async fn the_batch_write_indexes_every_spend_at_its_location() {
    let (_temp_dir, db) = empty_store().await;
    let blocks = vector_chain();

    tokio::task::block_in_place(|| db.write_block_batch_blocking(&blocks))
        .expect("the vector chain writes as one batch");

    assert_spent_index_is(&db, &expected_spent_index(&blocks));
}

// multi_thread required: the write and delete paths run LMDB work through `block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_tip_removes_its_spent_rows_and_no_others() {
    let (_temp_dir, db) = empty_store().await;
    let blocks = vector_chain();
    for block in blocks.iter().cloned() {
        db.write_block(block).await.expect("a vector block writes");
    }
    let (tip, below_tip) = blocks.split_last().expect("the vector chain is not empty");
    assert!(
        !expected_spent_index(std::slice::from_ref(tip)).is_empty(),
        "the vector tip spends nothing, so the test proves nothing"
    );

    db.delete_block_at_height(tip.context.index.height)
        .await
        .expect("the tip deletes");

    assert_spent_index_is(&db, &expected_spent_index(below_tip));
}

/// The vector tip re-addressed to sit two heights above itself while still naming the tip as its parent, so only the height gap can refuse it.
fn block_two_above(tip: &IndexedBlock<AbsoluteChainWork>) -> IndexedBlock<AbsoluteChainWork> {
    let mut gapped = tip.clone();
    gapped.context = crate::types::BlockContext::new(
        *tip.context.hash(),
        *tip.context.hash(),
        tip.context.chainwork(),
        Height(tip.context.index.height.0 + 2),
    );
    gapped
}

// multi_thread required: the write path runs LMDB work through `block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn a_block_two_above_the_tip_is_refused_as_not_extending_it() {
    let (_temp_dir, db) = empty_store().await;
    let blocks = vector_chain();
    for block in blocks.iter().cloned() {
        db.write_block(block).await.expect("a vector block writes");
    }
    let tip = blocks.last().expect("the vector chain is not empty");
    let gapped = block_two_above(tip);
    let offered = gapped.context.index.height.0;

    let refused = db.write_block(gapped).await;

    assert!(
        matches!(
            refused,
            Err(StoreError::DoesNotExtendTip { height, tip: stored, .. })
                if height == offered && stored == *tip.context.hash()
        ),
        "expected DoesNotExtendTip at {offered}, got {refused:?}"
    );
    assert_eq!(
        db.tip_height().await.expect("the tip reads"),
        Some(tip.context.index.height),
        "the refused block leaves the tip where it was"
    );
}

// multi_thread required: the batch write runs LMDB work through `block_in_place`.
#[cfg(not(feature = "transparent_address_history_experimental"))]
#[tokio::test(flavor = "multi_thread")]
async fn the_batch_write_refuses_a_block_two_above_the_tip() {
    let (_temp_dir, db) = empty_store().await;
    let blocks = vector_chain();
    tokio::task::block_in_place(|| db.write_block_batch_blocking(&blocks))
        .expect("the vector chain writes as one batch");
    let tip = blocks.last().expect("the vector chain is not empty");
    let gapped = block_two_above(tip);
    let offered = gapped.context.index.height.0;

    let refused = tokio::task::block_in_place(|| db.write_block_batch_blocking(&[gapped]));

    assert!(
        matches!(
            refused,
            Err(StoreError::DoesNotExtendTip { height, tip: stored, .. })
                if height == offered && stored == *tip.context.hash()
        ),
        "expected DoesNotExtendTip at {offered}, got {refused:?}"
    );
}
