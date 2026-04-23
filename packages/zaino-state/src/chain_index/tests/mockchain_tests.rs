use super::load_test_vectors_and_sync_chain_index;
use tokio::time::{sleep, Duration};
use tokio_stream::StreamExt as _;
use zebra_chain::serialization::ZcashDeserializeInto;

use crate::chain_index::{
    tests::vectors::TestVectorBlockData,
    types::{BestChainLocation, TransactionHash},
    ChainIndex,
};

#[tokio::test(flavor = "multi_thread")]
async fn get_block_range() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(false).await;
    let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    let start = crate::Height(0);

    let indexer_blocks =
        ChainIndex::get_block_range(&index_reader, &nonfinalized_snapshot, start, None)
            .unwrap()
            .collect::<Vec<_>>()
            .await;

    for (i, block) in indexer_blocks.into_iter().enumerate() {
        let parsed_block = block
            .unwrap()
            .zcash_deserialize_into::<zebra_chain::block::Block>()
            .unwrap();

        let expected_block = &blocks[i].zebra_block;
        assert_eq!(&parsed_block, expected_block);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_raw_transaction() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(false).await;
    let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    for (expected_transaction, height) in blocks.into_iter().flat_map(|block| {
        block
            .zebra_block
            .transactions
            .into_iter()
            .map(move |transaction| (transaction, block.height))
    }) {
        let (transaction, branch_id) = index_reader
            .get_raw_transaction(
                &nonfinalized_snapshot,
                &TransactionHash::from(expected_transaction.hash()),
            )
            .await
            .unwrap()
            .unwrap();
        let zaino_transaction = transaction
            .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
            .unwrap();
        assert_eq!(expected_transaction.as_ref(), &zaino_transaction);
        assert_eq!(
            branch_id,
            if height == 0 {
                None
            } else if height == 1 {
                zebra_chain::parameters::NetworkUpgrade::Canopy
                    .branch_id()
                    .map(u32::from)
            } else {
                zebra_chain::parameters::NetworkUpgrade::Nu6_1
                    .branch_id()
                    .map(u32::from)
            }
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_transaction_status() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(false).await;
    let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    for (expected_transaction, block_hash, block_height) in blocks.into_iter().flat_map(|block| {
        block
            .zebra_block
            .transactions
            .iter()
            .cloned()
            .map(|transaction| {
                (
                    transaction,
                    block.zebra_block.hash(),
                    block.zebra_block.coinbase_height(),
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
    }) {
        let expected_txid = expected_transaction.hash();

        let (transaction_status_best_chain, transaction_status_nonbest_chain) = index_reader
            .get_transaction_status(
                &nonfinalized_snapshot,
                &TransactionHash::from(expected_txid),
            )
            .await
            .unwrap();
        assert!(transaction_status_nonbest_chain.is_empty());
        assert_eq!(
            transaction_status_best_chain.unwrap(),
            BestChainLocation::Block(
                crate::BlockHash(block_hash.0),
                crate::Height(block_height.unwrap().0)
            )
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sync_blocks_after_startup() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    let indexer_tip = dbg!(
        &index_reader
            .snapshot_nonfinalized_state()
            .await
            .unwrap()
            .get_nfs_snapshot()
            .unwrap()
            .best_tip
    )
    .height
    .0;
    let active_mockchain_tip = dbg!(mockchain.active_height());
    assert_eq!(active_mockchain_tip, indexer_tip);

    for _ in 0..20 {
        mockchain.mine_blocks(1);
        sleep(Duration::from_millis(600)).await;
    }
    sleep(Duration::from_millis(2000)).await;

    let indexer_tip = dbg!(
        &index_reader
            .snapshot_nonfinalized_state()
            .await
            .unwrap()
            .get_nfs_snapshot()
            .unwrap()
            .best_tip
    )
    .height
    .0;
    let active_mockchain_tip = dbg!(mockchain.active_height());
    assert_eq!(active_mockchain_tip, indexer_tip);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_transaction() {
    let (blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;
    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    sleep(Duration::from_millis(2000)).await;

    let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;

    let mempool_transactions: Vec<_> = block_data
        .get(mempool_height)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    for expected_transaction in mempool_transactions.into_iter() {
        let (transaction, branch_id) = index_reader
            .get_raw_transaction(
                &nonfinalized_snapshot,
                &TransactionHash::from(expected_transaction.hash()),
            )
            .await
            .unwrap()
            .unwrap();
        let zaino_transaction = transaction
            .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
            .unwrap();
        assert_eq!(expected_transaction.as_ref(), &zaino_transaction);
        assert_eq!(
            branch_id,
            zebra_chain::parameters::NetworkUpgrade::Nu6_1
                .branch_id()
                .map(u32::from)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_transaction_status() {
    let (blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;
    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    sleep(Duration::from_millis(2000)).await;

    let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;

    let mempool_transactions: Vec<_> = block_data
        .get(mempool_height)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
    for expected_transaction in mempool_transactions.into_iter() {
        let expected_txid = expected_transaction.hash();

        let (transaction_status_best_chain, transaction_status_nonbest_chain) = index_reader
            .get_transaction_status(
                &nonfinalized_snapshot,
                &TransactionHash::from(expected_txid),
            )
            .await
            .unwrap();
        assert_eq!(
            transaction_status_best_chain,
            Some(BestChainLocation::Mempool(
                crate::chain_index::types::Height(mempool_height as u32)
            ))
        );
        assert!(transaction_status_nonbest_chain.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_transactions() {
    let (blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;
    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    sleep(Duration::from_millis(2000)).await;

    let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
    let mut mempool_transactions: Vec<_> = block_data
        .get(mempool_height)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    mempool_transactions.sort_by_key(|a| a.hash());

    let mut found_mempool_transactions: Vec<zebra_chain::transaction::Transaction> = index_reader
        .get_mempool_transactions(Vec::new())
        .await
        .unwrap()
        .iter()
        .map(|txn_bytes| {
            txn_bytes
                .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                .unwrap()
        })
        .collect();
    found_mempool_transactions.sort_by_key(|a| a.hash());
    assert_eq!(
        mempool_transactions
            .iter()
            .map(|tx| tx.as_ref().clone())
            .collect::<Vec<_>>(),
        found_mempool_transactions,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_filtered_mempool_transactions() {
    let (blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;
    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    sleep(Duration::from_millis(2000)).await;

    let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
    let mut mempool_transactions: Vec<_> = block_data
        .get(mempool_height)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let exclude_tx = mempool_transactions.pop().unwrap();
    let exclude_txid = exclude_tx.hash().to_string();
    mempool_transactions.sort_by_key(|a| a.hash());

    let mut found_mempool_transactions: Vec<zebra_chain::transaction::Transaction> = index_reader
        .get_mempool_transactions(vec![exclude_txid])
        .await
        .unwrap()
        .iter()
        .map(|txn_bytes| {
            txn_bytes
                .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                .unwrap()
        })
        .collect();
    found_mempool_transactions.sort_by_key(|a| a.hash());
    assert_eq!(mempool_transactions.len(), found_mempool_transactions.len());
    assert_eq!(
        mempool_transactions
            .iter()
            .map(|tx| tx.as_ref().clone())
            .collect::<Vec<_>>(),
        found_mempool_transactions,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn get_mempool_stream_no_expected_chain_tip_snapshot() {
    let (blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    sleep(Duration::from_millis(2000)).await;

    let next_mempool_height_index = (dbg!(mockchain.active_height()) as usize) + 1;
    let mut mempool_transactions: Vec<_> = block_data
        .get(next_mempool_height_index)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    mempool_transactions.sort_by_key(|transaction| transaction.hash());

    let mempool_stream_task = tokio::spawn(async move {
        let mut mempool_stream = index_reader
            .get_mempool_stream(None)
            .expect("failed to create mempool stream");

        let mut indexer_mempool_transactions: Vec<zebra_chain::transaction::Transaction> =
            Vec::new();

        while let Some(tx_bytes_res) = mempool_stream.next().await {
            let tx_bytes = tx_bytes_res.expect("stream error");
            let tx: zebra_chain::transaction::Transaction =
                tx_bytes.zcash_deserialize_into().expect("deserialize tx");
            indexer_mempool_transactions.push(tx);
        }

        indexer_mempool_transactions.sort_by_key(|tx| tx.hash());
        indexer_mempool_transactions
    });

    sleep(Duration::from_millis(500)).await;

    mockchain.mine_blocks(1);

    let indexer_mempool_stream_transactions =
        mempool_stream_task.await.expect("collector task failed");

    assert_eq!(
        mempool_transactions
            .iter()
            .map(|tx| tx.as_ref().clone())
            .collect::<Vec<_>>(),
        indexer_mempool_stream_transactions,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn get_mempool_stream_correct_expected_chain_tip_snapshot() {
    let (blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;

    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    sleep(Duration::from_millis(2000)).await;

    let next_mempool_height_index = (dbg!(mockchain.active_height()) as usize) + 1;
    let mut mempool_transactions: Vec<_> = block_data
        .get(next_mempool_height_index)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    mempool_transactions.sort_by_key(|transaction| transaction.hash());

    let mempool_stream_task = tokio::spawn(async move {
        let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
        let mut mempool_stream = index_reader
            .get_mempool_stream(Some(&nonfinalized_snapshot))
            .expect("failed to create mempool stream");

        let mut indexer_mempool_transactions: Vec<zebra_chain::transaction::Transaction> =
            Vec::new();

        while let Some(tx_bytes_res) = mempool_stream.next().await {
            let tx_bytes = tx_bytes_res.expect("stream error");
            let tx: zebra_chain::transaction::Transaction =
                tx_bytes.zcash_deserialize_into().expect("deserialize tx");
            indexer_mempool_transactions.push(tx);
        }

        indexer_mempool_transactions.sort_by_key(|tx| tx.hash());
        indexer_mempool_transactions
    });

    sleep(Duration::from_millis(500)).await;

    mockchain.mine_blocks(1);

    let indexer_mempool_stream_transactions =
        mempool_stream_task.await.expect("collector task failed");

    assert_eq!(
        mempool_transactions
            .iter()
            .map(|tx| tx.as_ref().clone())
            .collect::<Vec<_>>(),
        indexer_mempool_stream_transactions,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn get_mempool_stream_for_stale_snapshot() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(true).await;
    sleep(Duration::from_millis(2000)).await;

    let stale_nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    mockchain.mine_blocks(1);
    sleep(Duration::from_millis(2000)).await;

    let mempool_stream = index_reader.get_mempool_stream(Some(&stale_nonfinalized_snapshot));

    assert!(mempool_stream.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_block_height() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(false).await;
    let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    // Positive cases: every known best-chain block returns its height
    for TestVectorBlockData {
        height,
        zebra_block,
        ..
    } in blocks.iter()
    {
        let got = index_reader
            .get_block_height(
                &nonfinalized_snapshot,
                crate::BlockHash(zebra_block.hash().0),
            )
            .await
            .unwrap();
        assert_eq!(got, Some(crate::Height(*height)));
    }

    // Negative case: an unknown hash returns None
    let unknown = crate::BlockHash([0u8; 32]);
    let got = index_reader
        .get_block_height(&nonfinalized_snapshot, unknown)
        .await
        .unwrap();
    assert_eq!(got, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_treestate() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(false).await;

    for TestVectorBlockData {
        zebra_block,
        sapling_tree_state,
        orchard_tree_state,
        ..
    } in blocks.into_iter()
    {
        let (sapling_bytes_opt, orchard_bytes_opt) = index_reader
            .get_treestate(&crate::BlockHash(zebra_block.hash().0))
            .await
            .unwrap();

        assert_eq!(
            sapling_bytes_opt.as_deref(),
            Some(sapling_tree_state.as_slice())
        );
        assert_eq!(
            orchard_bytes_opt.as_deref(),
            Some(orchard_tree_state.as_slice())
        );
    }
}
