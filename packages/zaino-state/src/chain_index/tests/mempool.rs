//! Zaino-State ChainIndex Mempool unit tests.

use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    str::FromStr as _,
};
use tokio::time::{timeout, Duration};
use zebra_chain::serialization::ZcashDeserialize as _;

use crate::{
    chain_index::{
        mempool::MempoolSubscriber,
        source::test::MockchainSource,
        tests::{
            poll::poll_until,
            vectors::{build_active_mockchain_source, load_test_vectors, TestVectorBlockData},
        },
    },
    BlockHash, BlockchainSource as _, Mempool, MempoolKey, MempoolValue,
};

/// Poll the subscriber until its mempool keys exactly match `expected_txids`.
///
/// Mockchain mempool is deterministic — non-coinbase txs of block[tip+1] — so
/// once the Mempool sync task has caught up to a mined tip, the subscriber's
/// keys equal the expected set. Waiting for that exact match replaces fixed
/// `sleep` pads and fails loud on drift.
async fn wait_for_mempool_to_reflect(
    subscriber: &MempoolSubscriber,
    expected_txids: impl IntoIterator<Item = String>,
) {
    let expected: HashSet<String> = expected_txids.into_iter().collect();
    poll_until(
        "mempool to reflect expected txids",
        Duration::from_secs(5),
        Duration::from_millis(50),
        || async {
            let got: HashSet<String> = subscriber
                .get_mempool()
                .await
                .into_iter()
                .map(|(MempoolKey { txid }, _)| txid)
                .collect();
            (got == expected).then_some(())
        },
    )
    .await;
}

/// Poll the subscriber until its `mempool_chain_tip` equals `expected`.
///
/// The mempool propagates new tips via a `watch::Sender<BlockHash>` updated
/// from its background sync task. Waiters that only check transaction ids
/// (see [`wait_for_mempool_to_reflect`]) can observe the new txs before the
/// tip-hash channel is updated, so callers about to pass an expected tip to
/// `get_mempool_stream` must wait on this helper separately.
async fn wait_for_mempool_tip_hash(subscriber: &MempoolSubscriber, expected: BlockHash) {
    poll_until(
        "mempool chain tip to match expected hash",
        Duration::from_secs(5),
        Duration::from_millis(50),
        || async { (subscriber.mempool_chain_tip() == expected).then_some(()) },
    )
    .await;
}

async fn spawn_mempool_and_mockchain() -> (
    Mempool<MockchainSource>,
    MempoolSubscriber,
    MockchainSource,
    Vec<zebra_chain::block::Block>,
) {
    let blocks = load_test_vectors().unwrap().blocks;

    let mockchain = build_active_mockchain_source(0, blocks.clone());

    let mempool = Mempool::spawn(mockchain.clone(), None).await.unwrap();

    let subscriber = mempool.subscriber();

    let block_data = blocks
        .iter()
        .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
        .collect();

    (mempool, subscriber, mockchain, block_data)
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool() {
    let (_mempool, subscriber, mockchain, block_data) = spawn_mempool_and_mockchain().await;

    let mut active_chain_height = dbg!(mockchain.active_height());
    assert_eq!(active_chain_height, 0);
    let max_chain_height = mockchain.max_chain_height();

    for _ in 0..=max_chain_height {
        let mempool_index = (active_chain_height as usize) + 1;
        let mempool_transactions = block_data
            .get(mempool_index)
            .map(|b| {
                b.transactions
                    .iter()
                    .filter(|tx| !tx.is_coinbase())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        wait_for_mempool_to_reflect(
            &subscriber,
            mempool_transactions.iter().map(|tx| tx.hash().to_string()),
        )
        .await;

        let subscriber_tx = subscriber.get_mempool().await;

        for transaction in mempool_transactions.into_iter() {
            let transaction_hash = dbg!(transaction.hash());

            let (subscriber_tx_hash, subscriber_tx) = subscriber_tx
                .iter()
                .find(|(k, _)| k.txid == transaction_hash.to_string())
                .map(
                    |(MempoolKey { txid: s }, MempoolValue { serialized_tx: tx })| {
                        (
                            zebra_chain::transaction::Hash::from_str(s).unwrap(),
                            tx.clone(),
                        )
                    },
                )
                .unwrap();

            let subscriber_transaction = zebra_chain::transaction::Transaction::zcash_deserialize(
                Cursor::new(subscriber_tx.as_ref()),
            )
            .unwrap();

            assert_eq!(transaction_hash, subscriber_tx_hash);
            assert_eq!(*transaction, subscriber_transaction);
        }

        if active_chain_height < max_chain_height {
            mockchain.mine_blocks(10);
            active_chain_height = dbg!(mockchain.active_height());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_filtered_mempool() {
    let (_mempool, subscriber, mockchain, block_data) = spawn_mempool_and_mockchain().await;

    mockchain.mine_blocks(150);
    let active_chain_height = mockchain.active_height();

    let mempool_index = (active_chain_height as usize) + 1;
    let mempool_transactions = block_data
        .get(mempool_index)
        .map(|b| b.transactions.clone())
        .unwrap_or_default();

    wait_for_mempool_to_reflect(
        &subscriber,
        mempool_transactions
            .iter()
            .filter(|tx| !tx.is_coinbase())
            .map(|tx| tx.hash().to_string()),
    )
    .await;

    let exclude_hash = mempool_transactions[0].hash();

    let subscriber_tx = subscriber
        .get_filtered_mempool(vec![exclude_hash.to_string()])
        .await;

    println!("Checking transactions..");

    for transaction in mempool_transactions.into_iter() {
        let transaction_hash = transaction.hash();
        if transaction_hash == exclude_hash {
            // check tx is *not* in mempool transactions
            let maybe_subscriber_tx = subscriber_tx
                .iter()
                .find(|(k, _)| k.txid == transaction_hash.to_string())
                .map(
                    |(MempoolKey { txid: s }, MempoolValue { serialized_tx: tx })| {
                        (
                            zebra_chain::transaction::Hash::from_str(s).unwrap(),
                            tx.clone(),
                        )
                    },
                );

            assert!(maybe_subscriber_tx.is_none());
        } else {
            let (subscriber_tx_hash, subscriber_tx) = subscriber_tx
                .iter()
                .find(|(k, _)| k.txid == transaction_hash.to_string())
                .map(
                    |(MempoolKey { txid: s }, MempoolValue { serialized_tx: tx })| {
                        (
                            zebra_chain::transaction::Hash::from_str(s).unwrap(),
                            tx.clone(),
                        )
                    },
                )
                .unwrap();

            let subscriber_transaction = zebra_chain::transaction::Transaction::zcash_deserialize(
                Cursor::new(subscriber_tx.as_ref()),
            )
            .unwrap();

            assert_eq!(transaction_hash, subscriber_tx_hash);
            assert_eq!(*transaction, subscriber_transaction);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_transaction() {
    let (_mempool, subscriber, mockchain, block_data) = spawn_mempool_and_mockchain().await;

    mockchain.mine_blocks(150);
    let active_chain_height = dbg!(mockchain.active_height());

    let mempool_index = (active_chain_height as usize) + 1;

    let mempool_transactions: Vec<_> = block_data
        .get(mempool_index)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    wait_for_mempool_to_reflect(
        &subscriber,
        mempool_transactions.iter().map(|tx| tx.hash().to_string()),
    )
    .await;

    let target_transaction = mempool_transactions
        .first()
        .expect("expected at least one non-coinbase mempool transaction");
    let target_hash = target_transaction.hash();

    let subscriber_tx = subscriber
        .get_transaction(&MempoolKey {
            txid: target_hash.to_string(),
        })
        .await
        .unwrap()
        .serialized_tx
        .clone();

    let subscriber_transaction = zebra_chain::transaction::Transaction::zcash_deserialize(
        Cursor::new(subscriber_tx.as_ref()),
    )
    .unwrap();

    assert_eq!(*mempool_transactions[0], subscriber_transaction);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_info() {
    let (_mempool, subscriber, mockchain, block_data) = spawn_mempool_and_mockchain().await;

    mockchain.mine_blocks(150);
    let active_chain_height = dbg!(mockchain.active_height());

    let mempool_index = (active_chain_height as usize) + 1;

    // 1) Take the “next block” as a mempool proxy, but:
    //    - exclude coinbase
    //    - dedupe by txid (mempool is keyed by txid)
    let mut seen = HashSet::new();
    let mempool_transactions: Vec<_> = block_data
        .get(mempool_index)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .filter(|tx| seen.insert(tx.hash())) // returns true only on first insert
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    wait_for_mempool_to_reflect(
        &subscriber,
        mempool_transactions.iter().map(|tx| tx.hash().to_string()),
    )
    .await;

    let subscriber_mempool_info = subscriber.get_mempool_info().await;

    let expected_size: u64 = mempool_transactions.len() as u64;

    let expected_bytes: u64 = mempool_transactions
        .iter()
        .map(|tx| {
            // Mempool stores SerializedTransaction, so mirror that here.
            let st: zebra_chain::transaction::SerializedTransaction = tx.as_ref().into();
            st.as_ref().len() as u64
        })
        .sum();

    let expected_key_heap_bytes: u64 = mempool_transactions
        .iter()
        .map(|tx| {
            // Keys are hex txid strings; measure heap capacity like the implementation.
            tx.hash().to_string().capacity() as u64
        })
        .sum();

    let expected_usage: u64 = expected_bytes + expected_key_heap_bytes;

    assert_eq!(subscriber_mempool_info.size, expected_size, "size mismatch");
    assert_eq!(
        subscriber_mempool_info.bytes, expected_bytes,
        "bytes mismatch"
    );
    assert_eq!(
        subscriber_mempool_info.usage, expected_usage,
        "usage mismatch"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_stream_no_expected_chain_tip() {
    let (_mempool, subscriber, mockchain, block_data) = spawn_mempool_and_mockchain().await;
    let mut subscriber = subscriber;

    mockchain.mine_blocks(150);
    let active_chain_height = dbg!(mockchain.active_height());

    let mempool_index = (active_chain_height as usize) + 1;

    let mempool_transactions: Vec<_> = block_data
        .get(mempool_index)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    wait_for_mempool_to_reflect(
        &subscriber,
        mempool_transactions.iter().map(|tx| tx.hash().to_string()),
    )
    .await;

    let (mut rx, handle) = subscriber.get_mempool_stream(None).await.unwrap();

    let expected_count = mempool_transactions.len();
    let mut received: HashMap<String, Vec<u8>> = HashMap::new();

    let collect_deadline = Duration::from_secs(2);

    timeout(collect_deadline, async {
        while received.len() < expected_count {
            match rx.recv().await {
                Some(Ok((MempoolKey { txid: k }, MempoolValue { serialized_tx: v }))) => {
                    received.insert(k, v.as_ref().as_ref().to_vec());
                }
                Some(Err(e)) => panic!("stream yielded error: {e:?}"),
                None => break,
            }
        }
    })
    .await
    .expect("timed out waiting for initial mempool stream items");

    let expected: HashMap<String, Vec<u8>> = mempool_transactions
        .iter()
        .map(|tx| {
            let key = tx.hash().to_string();
            let st: zebra_chain::transaction::SerializedTransaction = tx.as_ref().into();
            (key, st.as_ref().to_vec())
        })
        .collect();

    assert_eq!(received.len(), expected.len(), "entry count mismatch");
    for (k, bytes) in expected.iter() {
        let got = received
            .get(k)
            .unwrap_or_else(|| panic!("missing tx {k} in stream"));
        assert_eq!(got, bytes, "bytes mismatch for {k}");
    }

    mockchain.mine_blocks(1);

    timeout(Duration::from_secs(5), async {
        while let Some(_msg) = rx.recv().await {}
    })
    .await
    .expect("mempool stream did not close after mining a new block");

    handle.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_stream_correct_expected_chain_tip() {
    let (_mempool, subscriber, mockchain, block_data) = spawn_mempool_and_mockchain().await;
    let mut subscriber = subscriber;

    mockchain.mine_blocks(150);
    let active_chain_tip_height = dbg!(mockchain.active_height());
    let active_chain_tip_hash = mockchain.get_best_block_hash().await.unwrap().unwrap();

    let mempool_index = (active_chain_tip_height as usize) + 1;

    let mempool_transactions: Vec<_> = block_data
        .get(mempool_index)
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !tx.is_coinbase())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    wait_for_mempool_to_reflect(
        &subscriber,
        mempool_transactions.iter().map(|tx| tx.hash().to_string()),
    )
    .await;
    wait_for_mempool_tip_hash(&subscriber, active_chain_tip_hash.into()).await;

    let (mut rx, handle) = subscriber
        .get_mempool_stream(Some(active_chain_tip_hash.into()))
        .await
        .unwrap();

    let expected_count = mempool_transactions.len();
    let mut received: HashMap<String, Vec<u8>> = HashMap::new();

    let collect_deadline = Duration::from_secs(2);

    timeout(collect_deadline, async {
        while received.len() < expected_count {
            match rx.recv().await {
                Some(Ok((MempoolKey { txid: k }, MempoolValue { serialized_tx: v }))) => {
                    received.insert(k, v.as_ref().as_ref().to_vec());
                }
                Some(Err(e)) => panic!("stream yielded error: {e:?}"),
                None => break,
            }
        }
    })
    .await
    .expect("timed out waiting for initial mempool stream items");

    let expected: HashMap<String, Vec<u8>> = mempool_transactions
        .iter()
        .map(|tx| {
            let key = tx.hash().to_string();
            let st: zebra_chain::transaction::SerializedTransaction = tx.as_ref().into();
            (key, st.as_ref().to_vec())
        })
        .collect();

    assert_eq!(received.len(), expected.len(), "entry count mismatch");
    for (k, bytes) in expected.iter() {
        let got = received
            .get(k)
            .unwrap_or_else(|| panic!("missing tx {k} in stream"));
        assert_eq!(got, bytes, "bytes mismatch for {k}");
    }

    mockchain.mine_blocks(1);

    timeout(Duration::from_secs(5), async {
        while let Some(_msg) = rx.recv().await {}
    })
    .await
    .expect("mempool stream did not close after mining a new block");

    handle.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_stream_stale_expected_chain_tip() {
    let (_mempool, subscriber, mockchain, block_data) = spawn_mempool_and_mockchain().await;
    let mut subscriber = subscriber;

    // Wait for mempool to reflect block N+1's non-coinbase txs — proves the
    // sync task caught up to the mined tip. Use this as a steady-state signal
    // before advancing again.
    let next_block_txids = |tip_height: u32| -> Vec<String> {
        block_data
            .get(tip_height as usize + 1)
            .map(|b| {
                b.transactions
                    .iter()
                    .filter(|tx| !tx.is_coinbase())
                    .map(|tx| tx.hash().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    mockchain.mine_blocks(149);
    let state_chain_tip_hash = mockchain.get_best_block_hash().await.unwrap().unwrap();
    wait_for_mempool_to_reflect(&subscriber, next_block_txids(mockchain.active_height())).await;

    mockchain.mine_blocks(1);
    wait_for_mempool_to_reflect(&subscriber, next_block_txids(mockchain.active_height())).await;

    let result = subscriber
        .get_mempool_stream(Some(state_chain_tip_hash.into()))
        .await;

    match result {
        Err(crate::error::MempoolError::IncorrectChainTip {
            expected_chain_tip,
            current_chain_tip,
        }) => {
            assert_eq!(expected_chain_tip, state_chain_tip_hash);
            assert_ne!(current_chain_tip, state_chain_tip_hash);
        }
        Ok((_rx, handle)) => {
            handle.abort();
            panic!("expected IncorrectChainTip error, got Ok");
        }
        Err(other) => {
            panic!("expected IncorrectChainTip error, got {other:?}");
        }
    }
}
