use super::{load_test_vectors_and_sync_chain_index, MockchainMode};
use crate::{
    chain_index::{
        source::mockchain_source::MockchainSource,
        tests::{
            poll::poll_until,
            vectors::{indexed_block_chain, load_test_vectors, TestVectorBlockData},
        },
        types::{BestChainLocation, ChainScope, TransactionHash},
        ChainIndex, ChainIndexRpcExt, NodeBackedChainIndexSubscriber,
    },
    BlockchainSource as _, Outpoint,
};
use tokio::time::Duration;
use tokio_stream::StreamExt as _;
use zaino_fetch::jsonrpsee::response::address_deltas::{
    GetAddressDeltasParams, GetAddressDeltasResponse,
};
use zaino_fetch::jsonrpsee::response::block_header::GetBlockHeader;
use zebra_chain::serialization::{ZcashDeserializeInto, ZcashSerialize as _};
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};
use zebra_rpc::methods::GetBlock;
use zebra_state::HashOrHeight;

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
                zebra_chain::parameters::NetworkUpgrade::Nu6_2
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
            zebra_chain::parameters::NetworkUpgrade::Nu6_2
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

    // Same ordering constraint as the expected-tip variant below: the stream
    // must open before the mine, or it observes the drained post-mine
    // mempool and collects nothing. Without an expected tip there is no
    // guard at open, so the lost race presents as an assertion failure
    // rather than a hang. The handshake makes the ordering deterministic.
    let (stream_opened_tx, stream_opened_rx) = tokio::sync::oneshot::channel();

    let mempool_stream_task = tokio::spawn(async move {
        let mut mempool_stream = index_reader
            .get_mempool_stream(None)
            .expect("failed to create mempool stream");
        stream_opened_tx
            .send(())
            .expect("the main task awaits the handshake");

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

    stream_opened_rx
        .await
        .expect("the collector task opens the stream");

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

    // The stream closes only when the chain tip moves away from the tip its
    // snapshot recorded, and the mine below is that one move: the snapshot
    // and stream-open must therefore happen strictly before the mine, or the
    // stream arms itself against the post-mine tip and waits forever for a
    // second mine that never comes. A handshake makes the ordering
    // deterministic where a sleep only made it likely on an idle machine.
    let (stream_opened_tx, stream_opened_rx) = tokio::sync::oneshot::channel();

    let mempool_stream_task = tokio::spawn(async move {
        let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
        let mut mempool_stream = index_reader
            .get_mempool_stream(Some(&nonfinalized_snapshot))
            .expect("failed to create mempool stream");
        stream_opened_tx
            .send(())
            .expect("the main task awaits the handshake");

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

    stream_opened_rx
        .await
        .expect("the collector task opens the stream");

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
        let (sapling_bytes_opt, orchard_bytes_opt, _ironwood_bytes_opt) = index_reader
            .get_treestate(&crate::BlockHash(zebra_block.hash().0))
            .await
            .unwrap();

        assert_eq!(
            sapling_bytes_opt.map(|pool| pool.final_state),
            Some(sapling_tree_state)
        );
        assert_eq!(
            orchard_bytes_opt.map(|pool| pool.final_state),
            Some(orchard_tree_state)
        );
    }

    // Negative case: an unknown hash is rejected before proxying to the validator.
    let unknown = crate::BlockHash([0u8; 32]);
    let result = index_reader.get_treestate(&unknown).await;
    assert!(result.is_err());
    assert!(result
        .expect_err("unknown hash should be rejected")
        .message
        .contains("not found in local chain index"));
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

/// Walks zaino's own indexed view of the test-vector chain and derives, for every
/// non-coinbase transparent input, the `(outpoint, spending txid)` it represents, plus
/// every transparent outpoint created on the chain.
///
/// Ground truth is built from `CompactTxData` — the exact representation
/// `get_outpoint_spenders` scans — so the assertions also confirm the outpoint byte order
/// matches between an indexed input and the looked-up key.
fn outpoint_spend_ground_truth(
    blocks: &[TestVectorBlockData],
) -> (Vec<(Outpoint, TransactionHash)>, Vec<Outpoint>) {
    let mut spends = Vec::new();
    let mut created = Vec::new();
    for block in indexed_block_chain(blocks) {
        for tx in block.transactions() {
            let txid = *tx.txid();
            let transparent = tx.transparent();
            for output_index in 0..transparent.outputs().len() {
                created.push(Outpoint::new(txid.0, output_index as u32));
            }
            // Spend-walking goes through the canonical `spent_outpoints` helper (#1332),
            // whose null-prevout filtering and outpoint construction are pinned by its own
            // unit tests; here we only pair each spent outpoint with its spending txid.
            for outpoint in transparent.spent_outpoints() {
                spends.push((outpoint, txid));
            }
        }
    }
    (spends, created)
}

#[tokio::test(flavor = "multi_thread")]
async fn get_outpoint_spenders() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
    let snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    let (spends, created) = outpoint_spend_ground_truth(&blocks);
    assert!(
        !spends.is_empty(),
        "test vectors must contain transparent spends"
    );

    // Every spent outpoint resolves to its spending txid, index-aligned with the input.
    let outpoints: Vec<Outpoint> = spends.iter().map(|(op, _)| *op).collect();
    let result = index_reader
        .get_outpoint_spenders(&snapshot, outpoints, ChainScope::FullChain)
        .await
        .unwrap();
    assert_eq!(result.len(), spends.len());
    for ((outpoint, expected_txid), got) in spends.iter().zip(result) {
        assert_eq!(got, Some(*expected_txid), "wrong spender for {outpoint:?}");
    }

    // Outpoints that were created but never spent must report `None`.
    let spent_set: std::collections::HashSet<Outpoint> = spends.iter().map(|(op, _)| *op).collect();
    let unspent: Vec<Outpoint> = created
        .into_iter()
        .filter(|op| !spent_set.contains(op))
        .collect();
    assert!(!unspent.is_empty(), "expected some unspent outputs");
    let unspent_result = index_reader
        .get_outpoint_spenders(&snapshot, unspent.clone(), ChainScope::FullChain)
        .await
        .unwrap();
    assert_eq!(unspent_result.len(), unspent.len());
    assert!(unspent_result.iter().all(Option::is_none));
}

#[tokio::test(flavor = "multi_thread")]
async fn get_outpoint_spenders_empty_and_single() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
    let snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();

    // Empty input -> empty output.
    assert!(index_reader
        .get_outpoint_spenders(&snapshot, Vec::new(), ChainScope::FullChain)
        .await
        .unwrap()
        .is_empty());

    let (spends, created) = outpoint_spend_ground_truth(&blocks);

    // Length-1 query (the "single request" path) returns the expected spender.
    let (op, txid) = spends.first().unwrap();
    assert_eq!(
        index_reader
            .get_outpoint_spenders(&snapshot, vec![*op], ChainScope::FullChain)
            .await
            .unwrap(),
        vec![Some(*txid)],
    );

    // ...and a length-1 query for an unspent outpoint returns `None`.
    let spent_set: std::collections::HashSet<Outpoint> = spends.iter().map(|(op, _)| *op).collect();
    let unspent = created
        .into_iter()
        .find(|op| !spent_set.contains(op))
        .unwrap();
    assert_eq!(
        index_reader
            .get_outpoint_spenders(&snapshot, vec![unspent], ChainScope::FullChain)
            .await
            .unwrap(),
        vec![None],
    );
}

/// `z_get_block` served through the ChainIndex from the mock vectors: verbosity 0
/// round-trips to the stored block, and verbosity 1 matches the source and reports the
/// block's hash / height / txids.
#[tokio::test(flavor = "multi_thread")]
async fn z_get_block() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
    let active_height = mockchain.active_height();

    for height in [1u32, active_height / 2, active_height] {
        let id = HashOrHeight::Height(zebra_chain::block::Height(height));
        let expected_block = mockchain.get_block(id).await.unwrap().unwrap();

        // Verbosity 0: the raw serialized block round-trips to the stored block.
        let GetBlock::Raw(serialized) = index_reader
            .z_get_block(height.to_string(), Some(0))
            .await
            .unwrap()
        else {
            panic!("expected a raw block at verbosity 0");
        };
        let decoded: zebra_chain::block::Block =
            serialized.as_ref().zcash_deserialize_into().unwrap();
        assert_eq!(decoded, *expected_block);

        // Verbosity 1: the ChainIndex delegates to the source and the object reports the
        // block's hash, height, and every txid.
        let via_index = index_reader
            .z_get_block(height.to_string(), Some(1))
            .await
            .unwrap();
        let via_source = mockchain.get_block_verbose(id, Some(1)).await.unwrap();
        let value = serde_json::to_value(&via_index).unwrap();
        assert_eq!(value, serde_json::to_value(&via_source).unwrap());
        assert_eq!(
            value["hash"].as_str().unwrap(),
            expected_block.hash().to_string()
        );
        assert_eq!(value["height"].as_u64().unwrap(), u64::from(height));
        assert_eq!(
            value["tx"].as_array().unwrap().len(),
            expected_block.transactions.len()
        );
    }
}

/// `get_block_header` served through the ChainIndex from the mock vectors: the compact
/// (non-verbose) form round-trips to the stored header bytes, and the verbose form
/// matches the source and reports the right hash / height.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_header() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
    let active_height = mockchain.active_height();

    for height in [1u32, active_height / 2, active_height] {
        let id = HashOrHeight::Height(zebra_chain::block::Height(height));
        let block = mockchain.get_block(id).await.unwrap().unwrap();
        let hash = block.hash().to_string();

        // Non-verbose: the compact hex decodes to the block header's serialization.
        let GetBlockHeader::Compact(compact) = index_reader
            .get_block_header(hash.clone(), false)
            .await
            .unwrap()
        else {
            panic!("expected a compact header when verbose = false");
        };
        assert_eq!(
            hex::decode(compact).unwrap(),
            block.header.zcash_serialize_to_vec().unwrap()
        );

        // Verbose: the ChainIndex delegates to the source and reports hash / height.
        let via_index = index_reader
            .get_block_header(hash.clone(), true)
            .await
            .unwrap();
        let via_source = mockchain
            .get_block_header(hash.clone(), true)
            .await
            .unwrap();
        let value = serde_json::to_value(&via_index).unwrap();
        assert_eq!(value, serde_json::to_value(&via_source).unwrap());
        assert_eq!(value["hash"].as_str().unwrap(), hash);
        assert_eq!(value["height"].as_u64().unwrap(), u64::from(height));
    }
}

/// `get_block_deltas` served through the ChainIndex from the mock vectors: it matches the
/// source, reports the right block hash / height, and surfaces transparent deltas.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_deltas() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
    let active_height = mockchain.active_height();

    let mut saw_delta_entries = false;
    for height in [1u32, active_height / 2, active_height] {
        let id = HashOrHeight::Height(zebra_chain::block::Height(height));
        let block = mockchain.get_block(id).await.unwrap().unwrap();
        let hash = block.hash().to_string();

        let via_index = index_reader.get_block_deltas(hash.clone()).await.unwrap();
        let via_source = mockchain.get_block_deltas(hash.clone()).await.unwrap();
        assert_eq!(
            serde_json::to_value(&via_index).unwrap(),
            serde_json::to_value(&via_source).unwrap()
        );
        assert_eq!(via_index.hash, hash);
        assert_eq!(via_index.height, height);
        if via_index
            .deltas
            .iter()
            .any(|delta| !delta.inputs.is_empty() || !delta.outputs.is_empty())
        {
            saw_delta_entries = true;
        }
    }
    assert!(
        saw_delta_entries,
        "expected transparent deltas in at least one sampled block"
    );
}

/// `get_difficulty` served through the ChainIndex from the mock vectors matches the
/// source and is a positive difficulty value.
#[tokio::test(flavor = "multi_thread")]
async fn get_difficulty() {
    let (_blocks, _indexer, index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let via_index = index_reader.get_difficulty().await.unwrap();
    let via_source = mockchain.get_difficulty().await.unwrap();
    assert_eq!(via_index, via_source);
    assert!(
        via_index > 0.0,
        "difficulty should be positive, got {via_index}"
    );
}

/// Drives the merged [`NodeBackedIndexerServiceSubscriber`] RPC layer over a
/// `MockchainSource`, confirming the service delegates to its chain index: the
/// service's `get_latest_block` reports the same tip the mockchain was synced to.
#[tokio::test(flavor = "multi_thread")]
async fn node_backed_indexer_service_serves_latest_block() {
    use crate::indexer::node_backed_indexer::NodeBackedIndexerServiceSubscriber;
    use crate::LightWalletIndexer as _;
    use zaino_common::network::ActivationHeights;

    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let expected_tip = (blocks.len() as u32) - 1;
    wait_for_indexer_tip(&index_reader, expected_tip).await;

    let service = NodeBackedIndexerServiceSubscriber::new_for_test(
        index_reader,
        ActivationHeights::default().to_regtest_network(),
    );

    let latest = service.get_latest_block().await.unwrap();
    assert_eq!(latest.height, expected_tip as u64);
}

/// Dropping the chain index without an explicit `shutdown()` call must still
/// release source-owned background work: `Drop` previously only cancelled the
/// sync worker, and the async `shutdown()`'s `?` on the DB teardown skipped
/// the source release when the DB shutdown errored — either way the Direct
/// connection's Zebra syncer task could outlive its index.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_chain_index_releases_the_source() {
    let (_blocks, indexer, _index_reader, mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    assert!(
        !mockchain.shutdown_called(),
        "the source must not be shut down while the index is live"
    );
    drop(indexer);
    assert!(
        mockchain.shutdown_called(),
        "dropping the index must release source-owned background work"
    );
}

/// zebra's `getblock` resolves negative heights against the tip (`-1` is the
/// tip block). The old Rpc backend forwarded the raw identifier string to the
/// validator, so `getblock "-1"` worked; the merged pre-parse used
/// `HashOrHeight::from_str`, which rejects negative heights.
#[tokio::test(flavor = "multi_thread")]
async fn z_get_block_resolves_negative_heights_against_the_tip() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
    let tip_height = (blocks.len() as u32) - 1;
    wait_for_indexer_tip(&index_reader, tip_height).await;

    let by_negative_height = index_reader
        .z_get_block("-1".to_string(), Some(1))
        .await
        .expect("height -1 must resolve to the tip block");
    let by_tip_height = index_reader
        .z_get_block(tip_height.to_string(), Some(1))
        .await
        .expect("the tip height resolves");
    assert_eq!(by_negative_height, by_tip_height);
}

/// An unparsable `getblock` identifier must carry zcashd's legacy
/// InvalidParameter code (-8) as a typed `RpcError` in the `source()` chain,
/// not be flattened into an internal-error string: the serve layer recovers
/// legacy codes by downcast-walking the chain.
#[tokio::test(flavor = "multi_thread")]
async fn z_get_block_invalid_identifier_keeps_legacy_error_code() {
    let (blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;
    wait_for_indexer_tip(&index_reader, (blocks.len() as u32) - 1).await;

    let error = index_reader
        .z_get_block("notablockid".to_string(), Some(1))
        .await
        .expect_err("an unparsable identifier must be rejected");

    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut rpc_error_code = None;
    while let Some(source_error) = current {
        if let Some(rpc_error) =
            source_error.downcast_ref::<zaino_fetch::jsonrpsee::connector::RpcError>()
        {
            rpc_error_code = Some(rpc_error.code);
            break;
        }
        current = source_error.source();
    }
    assert_eq!(
        rpc_error_code,
        Some(zebra_rpc::server::error::LegacyCode::InvalidParameter as i64),
        "the typed RpcError (legacy code -8) must stay reachable via the source() chain"
    );
}

/// During the initial finalised-state build there is no non-finalised
/// snapshot. Both pre-merge backends answered `getchaintips` in that window by
/// proxying the validator's own response; the merged service must fall back to
/// the source the same way, rather than serving UnavailableNotSyncedEnough for
/// the whole build (hours on mainnet).
#[tokio::test]
async fn get_chain_tips_falls_back_to_source_while_syncing() {
    use crate::chain_index::non_finalised_state::ChainIndexSnapshot;
    use crate::chain_index::tests::vectors::build_mockchain_source;
    use crate::indexer::node_backed_indexer::chain_tips_for_snapshot;

    let blocks = load_test_vectors().unwrap().blocks;
    let tip_height = (blocks.len() as u32) - 1;
    let expected_tip_hash = blocks[tip_height as usize].zebra_block.hash().to_string();
    let mock = build_mockchain_source(blocks);

    let syncing_snapshot = ChainIndexSnapshot::StillSyncingFinalizedState {
        validator_finalized_height: crate::Height(tip_height),
    };

    let tips = chain_tips_for_snapshot(&syncing_snapshot, &mock)
        .await
        .expect("the syncing window must proxy the source's chain tips");

    assert_eq!(
        tips,
        vec![zaino_fetch::jsonrpsee::response::chain_tips::ChainTip::new(
            tip_height,
            expected_tip_hash,
            0,
            zaino_fetch::jsonrpsee::response::chain_tips::ChainTipStatus::Active,
        )]
    );
}

/// Dropping the service must not panic on a thread with no Tokio runtime.
/// `Drop` runs the blocking teardown, which previously called
/// `Handle::current()` unconditionally; outside a runtime that panics, and a
/// panic inside `Drop` during unwind aborts the process.
///
/// multi_thread required: the harness's finalised-state validation uses
/// `block_in_place`. The drop under test happens on a separate plain thread.
#[tokio::test(flavor = "multi_thread")]
async fn service_drop_survives_thread_without_runtime() {
    use crate::indexer::node_backed_indexer::NodeBackedIndexerService;
    use zaino_common::network::ActivationHeights;

    let (_blocks, indexer, _index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let service = NodeBackedIndexerService::new_for_test(
        indexer,
        ActivationHeights::default().to_regtest_network(),
    );

    std::thread::spawn(move || drop(service))
        .join()
        .expect("dropping the service off-runtime must not panic");
}

/// Dropping the service must not panic on a current-thread Tokio runtime,
/// where `block_in_place` (the old unconditional teardown entry) aborts.
///
/// multi_thread required: the harness's finalised-state validation uses
/// `block_in_place`. The drop under test happens inside a current-thread
/// runtime built on a separate thread.
#[tokio::test(flavor = "multi_thread")]
async fn service_drop_survives_current_thread_runtime() {
    use crate::indexer::node_backed_indexer::NodeBackedIndexerService;
    use zaino_common::network::ActivationHeights;

    let (_blocks, indexer, _index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let service = NodeBackedIndexerService::new_for_test(
        indexer,
        ActivationHeights::default().to_regtest_network(),
    );

    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime builds")
            .block_on(async move { drop(service) });
    })
    .join()
    .expect("dropping the service on a current-thread runtime must not panic");
}

/// The `Rpc` connection has no local chain-tip-change stream, so requesting a
/// chain-tip subscriber over such a source must yield `None` rather than
/// panic. Before this method returned `Option`, it existed only in a
/// panicking form (`.expect("chaintip_update_subscriber requires the Direct
/// connection")`) reachable by any embedder configured with `backend = "rpc"`;
/// pre-merge the misuse was a compile error because only the State-backed
/// subscriber type had the method.
#[tokio::test(flavor = "multi_thread")]
async fn chaintip_update_subscriber_absent_without_tip_stream() {
    use crate::indexer::node_backed_indexer::NodeBackedIndexerServiceSubscriber;
    use zaino_common::network::ActivationHeights;

    let (_blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let service = NodeBackedIndexerServiceSubscriber::new_for_test(
        index_reader,
        ActivationHeights::default().to_regtest_network(),
    );

    assert!(
        service.chaintip_update_subscriber().is_none(),
        "a source with no local tip-change stream must yield no subscriber, not panic"
    );
}

/// `sendrawtransaction` rejections must carry zcashd's legacy error code:
/// zaino-serve forwards the code by downcast-walking the `source()` chain for
/// the typed `RpcError` (`sendrawtransaction_error_object_from_indexer_error`),
/// so stringifying it downgrades the legacy `-8` "invalid hex" rejection to a
/// generic `-32603` internal error.
///
/// Invalid hex fails in local validation before the source is consulted, so
/// this also pins that the rejection happens without a validator round-trip
/// (the mock's `send_raw_transaction` would panic if reached).
#[tokio::test(flavor = "multi_thread")]
async fn send_raw_transaction_invalid_hex_keeps_legacy_error_code() {
    let (_blocks, _indexer, index_reader, _mockchain) =
        load_test_vectors_and_sync_chain_index(MockchainMode::Static).await;

    let error = index_reader
        .send_raw_transaction("notahexstring".to_string())
        .await
        .expect_err("invalid hex must be rejected");

    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    let mut rpc_error_code = None;
    while let Some(source_error) = current {
        if let Some(rpc_error) =
            source_error.downcast_ref::<zaino_fetch::jsonrpsee::connector::RpcError>()
        {
            rpc_error_code = Some(rpc_error.code);
            break;
        }
        current = source_error.source();
    }
    assert_eq!(
        rpc_error_code,
        Some(zebra_rpc::server::error::LegacyCode::InvalidParameter as i64),
        "the typed RpcError (legacy code -8) must stay reachable via the source() chain"
    );
}
