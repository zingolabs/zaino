use super::{load_test_vectors_and_sync_chain_index, MockchainMode};
use crate::{
    chain_index::{
        source::mockchain_source::MockchainSource,
        tests::{
            poll::poll_until,
            vectors::{load_test_vectors, TestVectorBlockData},
        },
        types::{BestChainLocation, TransactionHash},
        ChainIndex, NodeBackedChainIndexSubscriber,
    },
    BlockchainSource as _,
};
use tokio::time::{sleep, Duration};
use tokio_stream::StreamExt as _;
use zaino_fetch::jsonrpsee::response::address_deltas::{
    GetAddressDeltasParams, GetAddressDeltasResponse,
};
use zebra_chain::serialization::ZcashDeserializeInto;
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

/// Polls the indexer's nonfinalized-state snapshot until its best-tip height
/// equals `expected`, or panics after a 10 s budget.
///
/// Use this wherever a test previously relied on a fixed `sleep` to hope the
/// indexer's sync task had caught up with the mockchain tip: the indexer
/// publishes new tips asynchronously via its background loop, and under
/// full-suite parallel load those updates can lag well past 2 s.
async fn wait_for_indexer_tip(
    index_reader: &NodeBackedChainIndexSubscriber<MockchainSource>,
    expected: u32,
) {
    poll_until(
        "indexer tip to match expected height",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            let tip = index_reader
                .snapshot_nonfinalized_state()
                .await
                .ok()?
                .get_nfs_snapshot()?
                .best_tip
                .height
                .0;
            (tip == expected).then_some(())
        },
    )
    .await;
}

fn faucet_transparent_address() -> String {
    let vector_data = load_test_vectors().unwrap();

    let (transparent_address, _transaction_hash, _output_index, _script, _value, _height) =
        vector_data.faucet.utxos[0].clone().into_parts();

    transparent_address.to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn get_block_range() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

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
        wait_for_indexer_tip(&index_reader, mockchain.active_height()).await;
    }

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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;
    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    let mockchain_tip = mockchain.active_height();
    wait_for_indexer_tip(&index_reader, mockchain_tip).await;

    let mempool_height = (mockchain_tip as usize) + 1;

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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;
    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    let mockchain_tip = mockchain.active_height();
    wait_for_indexer_tip(&index_reader, mockchain_tip).await;

    let mempool_height = (mockchain_tip as usize) + 1;

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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;
    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    let mockchain_tip = mockchain.active_height();
    wait_for_indexer_tip(&index_reader, mockchain_tip).await;

    let mempool_height = (mockchain_tip as usize) + 1;
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;
    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    let mockchain_tip = mockchain.active_height();
    wait_for_indexer_tip(&index_reader, mockchain_tip).await;

    let mempool_height = (mockchain_tip as usize) + 1;
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    let mockchain_tip = mockchain.active_height();
    wait_for_indexer_tip(&index_reader, mockchain_tip).await;

    let next_mempool_height_index = (mockchain_tip as usize) + 1;
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;

    let block_data: Vec<zebra_chain::block::Block> = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    let mockchain_tip = mockchain.active_height();
    wait_for_indexer_tip(&index_reader, mockchain_tip).await;

    let next_mempool_height_index = (mockchain_tip as usize) + 1;
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Active).await;
    wait_for_indexer_tip(&index_reader, mockchain.active_height()).await;

    let stale_nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    mockchain.mine_blocks(1);
    wait_for_indexer_tip(&index_reader, mockchain.active_height()).await;

    // `wait_for_indexer_tip` only confirms the chain-index NFS has caught
    // up; the mempool serve loop polls `get_best_block_hash` on its own
    // independent timer and may still hold the pre-mine chain tip for a
    // tick or two. `get_mempool_stream(Some(stale))` returns `None` only
    // once the mempool's tracked tip has moved past the stale snapshot's
    // hash, so poll on that condition rather than asserting once.
    poll_until(
        "mempool to reject stale snapshot",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            index_reader
                .get_mempool_stream(Some(&stale_nonfinalized_snapshot))
                .is_none()
                .then_some(())
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn get_block_height() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
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
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

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

#[tokio::test(flavor = "multi_thread")]
async fn get_address_deltas() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let transparent_address = faucet_transparent_address();
    let active_height = mockchain.active_height();

    let expected_response = mockchain
        .get_address_deltas(GetAddressDeltasParams::new_filtered(
            vec![transparent_address.clone()],
            0,
            active_height,
            true,
        ))
        .await
        .unwrap();

    let indexer_response = index_reader
        .get_address_deltas(GetAddressDeltasParams::new_filtered(
            vec![transparent_address.clone()],
            0,
            active_height,
            true,
        ))
        .await
        .unwrap();

    assert_eq!(indexer_response, expected_response);

    match indexer_response {
        GetAddressDeltasResponse::WithChainInfo { deltas, start, end } => {
            assert!(!deltas.is_empty());
            assert_eq!(start.height, 0);
            assert_eq!(end.height, active_height);
        }
        GetAddressDeltasResponse::Simple(_) => {
            panic!("expected get_address_deltas response with chain info")
        }
    }

    let invalid_address_result = index_reader
        .get_address_deltas(GetAddressDeltasParams::new_address(
            "not_a_valid_transparent_address",
        ))
        .await;

    assert!(invalid_address_result.is_err());

    assert!(
        active_height > 0,
        "test requires a chain height greater than zero"
    );

    let invalid_range_result = index_reader
        .get_address_deltas(GetAddressDeltasParams::new_filtered(
            vec![transparent_address],
            active_height,
            active_height - 1,
            false,
        ))
        .await;

    assert!(invalid_range_result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_address_balance() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let transparent_address = faucet_transparent_address();

    let expected_balance = mockchain
        .get_address_balance(GetAddressBalanceRequest::new(vec![
            transparent_address.clone()
        ]))
        .await
        .unwrap();

    let indexer_balance = index_reader
        .get_address_balance(GetAddressBalanceRequest::new(vec![transparent_address]))
        .await
        .unwrap();

    assert_eq!(
        serde_json::to_value(indexer_balance).unwrap(),
        serde_json::to_value(expected_balance).unwrap()
    );

    let invalid_address_result = index_reader
        .get_address_balance(GetAddressBalanceRequest::new(vec![
            "not_a_valid_transparent_address".to_string(),
        ]))
        .await;

    assert!(invalid_address_result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_address_txids() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let transparent_address = faucet_transparent_address();
    let active_height = mockchain.active_height();

    let expected_txids = mockchain
        .get_address_txids(GetAddressTxIdsRequest::new(
            vec![transparent_address.clone()],
            Some(0),
            Some(active_height),
        ))
        .await
        .unwrap();

    let indexer_txids = index_reader
        .get_address_txids(GetAddressTxIdsRequest::new(
            vec![transparent_address],
            Some(0),
            Some(active_height),
        ))
        .await
        .unwrap();

    assert!(!indexer_txids.is_empty());
    assert_eq!(indexer_txids, expected_txids);

    let invalid_address_result = index_reader
        .get_address_txids(GetAddressTxIdsRequest::new(
            vec!["not_a_valid_transparent_address".to_string()],
            Some(0),
            Some(active_height),
        ))
        .await;

    assert!(invalid_address_result.is_err());

    let invalid_range_result = index_reader
        .get_address_txids(GetAddressTxIdsRequest::new(
            vec![faucet_transparent_address()],
            Some(active_height),
            Some(0),
        ))
        .await;

    assert!(invalid_range_result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_address_utxos() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let transparent_address = faucet_transparent_address();

    let expected_utxos = mockchain
        .get_address_utxos(GetAddressBalanceRequest::new(vec![
            transparent_address.clone()
        ]))
        .await
        .unwrap();

    let indexer_utxos = index_reader
        .get_address_utxos(GetAddressBalanceRequest::new(vec![transparent_address]))
        .await
        .unwrap();

    assert!(!indexer_utxos.is_empty());
    assert_eq!(indexer_utxos, expected_utxos);

    let invalid_address_result = index_reader
        .get_address_utxos(GetAddressBalanceRequest::new(vec![
            "not_a_valid_transparent_address".to_string(),
        ]))
        .await;

    assert!(invalid_address_result.is_err());
}
