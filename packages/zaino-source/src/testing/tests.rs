use std::sync::Arc;

use zaino_primitives::testing::h;
use zaino_primitives::types::MerkleRoot;

use super::*;
use crate::mock::fixture_transactions;

/// - links, polls and blocks by hash: its best chain only, the chain's own header bytes and info
/// - a reorg between its tip read and its `getblockhash` answers: one poll mixes both, the next
///   is whole
/// - a `raw_tx` served by its bytes once mined; a `TxBuilder` tx's body = panic, never invented
/// - each `Lie` breaks exactly what it names
#[tokio::test]
async fn a_validator_serves_its_best_chain_reorgs_mid_poll_and_lies_as_scripted() {
    let mut chain = MockChain::regtest().varied_work();
    let (txid, raw) = raw_transaction(7, 0);
    chain.mine_empty(2);
    let three = chain.mine(|b| b.raw_tx(decoded(raw.clone())).tx(|t| t.txid([0x33; 32])));
    let four = chain.mine_empty(1);
    let validator = Arc::new(MockValidator::following(&chain, three));

    let links = validator.get_block_links(&[h(0), h(3), h(4)]).await.expect("reachable");
    let link = |at: BlockRef| Ok(BlockLink { header: chain.header_bytes(at.hash) });
    let above = Err(GetAtHeightError::HeightNotFound(h(4)));
    assert_eq!(links, [link(chain.genesis()), link(three), above]);
    let polled = validator.get_poll_reading(false, &[]).await.expect("reachable");
    assert_eq!(polled.info, chain.blockchain_info(three));
    let unserved = validator.get_block_by_hash(four.hash).await;
    let not_found =
        |answer| matches!(answer, Err(QueryError::Domain(GetBlockByHashError::NotFound(_))));
    assert!(not_found(unserved), "nothing above its tip");

    let fork = chain.fork(h(2)).outweigh().mine_empty(1).tip();
    validator.reorg_after_next_poll(&chain, fork);
    let raced = validator.get_poll_reading(false, &[h(2), h(3)]).await.expect("reachable");
    let held: Vec<Option<BlockHash>> =
        raced.held.iter().map(|held| held.as_ref().ok().copied()).collect();
    let after = vec![Some(chain.at(h(2)).hash), Some(fork.hash)];
    assert_eq!((raced.info.best_block_hash, held), (three.hash, after), "tip read, then reorged");
    let next = validator.get_poll_reading(false, &[]).await.expect("reachable");
    assert_eq!(next.info, chain.blockchain_info(fork));
    assert!(not_found(validator.get_block_by_hash(three.hash).await), "reorged away");

    validator.follow(&chain, three);
    let mined = validator.get_transaction(txid).await.expect("mined on its best");
    assert_eq!((mined.bytes, mined.location), (raw, TransactionLocation::BestChain(h(3))));
    let asking = Arc::clone(&validator);
    let built = TransactionId::from([0x33; 32]);
    let asked = tokio::spawn(async move { asking.get_transaction(built).await }).await;
    assert!(asked.is_err_and(|join| join.is_panic()), "no invented body for a TxBuilder tx");

    let txids = |block: &Block| block.transactions().iter().map(|tx| tx.txid).collect::<Vec<_>>();
    let shape = |block: &Block| {
        let header = block.header();
        let rebuilds = MerkleRoot::of_txids(&txids(block)) == Some(header.merkle_root);
        (header.hash == three.hash, header_hash(header) == header.hash, rebuilds, header.height)
    };
    #[rustfmt::skip]
    let lies = [
        (None,                    (true,  true, true,  h(3))),
        (Some(Lie::WrongBlock),   (false, true, true,  h(3))),
        (Some(Lie::Poisoned),     (true,  true, false, h(3))),
        (Some(Lie::Mutated),      (true,  true, false, h(3))),
        (Some(Lie::WrongHeight),  (true,  true, true,  h(4))),
    ];
    for (lie, expected) in lies {
        validator.lie(lie);
        let served = validator.get_block_by_hash(three.hash).await.expect("answers");
        assert_eq!(shape(&served), expected, "{lie:?}");
    }
}

/// Paused clock, one validator at height 1:
/// - mempool: inserted at its stated fee, a relayed tx at the fee its bytes leave, a mined tx gone
/// - verdicts: an unknown input refused, a scripted refusal, garbage = `Malformed`
/// - listing refusal, metadata timeouts, the estimate; latency per call; injected failures;
///   every call counted
#[tokio::test(start_paused = true)]
async fn a_validators_mempool_relay_metadata_and_failures_follow_the_script() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(1);
    let validator = MockValidator::following(&chain, tip);
    let (listed, listed_raw) = raw_transaction(1, 0);
    assert_eq!(validator.mempool_insert(listed_raw.clone(), 2_000), listed);
    let (sent, sent_raw) = raw_transaction(2, 0);
    assert_eq!(validator.send_raw_transaction(sent_raw.clone()).await.expect("relayed"), sent);
    let fee = |zats: u64| Zatoshis::new(zats).expect("in supply");
    let len = |raw: &[u8]| u32::try_from(raw.len()).expect("a small transaction");
    let mut both = vec![
        MempoolListed { txid: listed, fee: fee(2_000), encoded_len: len(&listed_raw) },
        MempoolListed { txid: sent, fee: fee(0), encoded_len: len(&sent_raw) },
    ];
    both.sort_by_key(|entry| entry.txid);
    let polled = validator.get_poll_reading(true, &[]).await.expect("reachable");
    assert_eq!(polled.listing, Ok(both));
    let metadata = polled.metadata.expect("asked");
    assert_eq!(metadata.peers.expect("answered"), []);
    let entry = MempoolListed { txid: sent, fee: fee(0), encoded_len: len(&sent_raw) };
    let bytes = validator.get_raw_mempool_transactions(&[entry]).await.expect("reachable");
    assert_eq!(bytes, [Ok(sent_raw.clone())]);

    let foreign = fixture_transactions(2_000_000).into_iter().skip(1).find(|raw| {
        !crate::decode_transaction(raw).expect("fixture").transparent.inputs.is_empty()
    });
    let refused = validator.send_raw_transaction(foreign.expect("a transparent spend")).await;
    let rejected = |answer: Result<TransactionId, QueryError<SendRawTransactionError>>,
                    why: &str| {
        matches!(answer, Err(QueryError::Domain(SendRawTransactionError::Rejected(reason))) if reason.starts_with(why))
    };
    assert!(rejected(refused, "missing input"), "prevout not on its best chain");
    validator.relay(Err(SendRawTransactionError::Rejected("fee too low".to_owned())));
    assert!(rejected(validator.send_raw_transaction(raw_transaction(3, 0).1).await, "fee too low"));
    validator.relay(Ok(()));
    let garbage = validator.send_raw_transaction(vec![9; 8]).await;
    assert!(matches!(garbage, Err(QueryError::Domain(SendRawTransactionError::Malformed(_)))));

    let mined = chain.mine(|b| b.raw_tx(decoded(sent_raw)));
    validator.follow(&chain, mined);
    let release = NodeRelease {
        build: "v6.4.2".to_owned(),
        user_agent: "/Zebra:6.4.2/".to_owned(),
        protocol_version: 170_140,
        end_of_service: EndOfService::NotEnforced,
    };
    validator.metadata(None, Some(release.clone()));
    validator.estimate(h(100));
    let polled = validator.get_poll_reading(true, &[]).await.expect("reachable");
    let only = MempoolListed { txid: listed, fee: fee(2_000), encoded_len: len(&listed_raw) };
    assert_eq!(polled.listing, Ok(vec![only]), "mined = out of the mempool");
    assert_eq!((polled.info.blocks, polled.info.estimated_height), (h(2), h(100)));
    let metadata = polled.metadata.expect("asked");
    assert_eq!(metadata.peers.expect_err("times out").mode, FailureMode::Timeout);
    assert_eq!(metadata.release.expect("answered"), release);
    validator.listing(Err(GetMempoolListingError::Inactive));
    let polled = validator.get_poll_reading(false, &[]).await.expect("reachable");
    assert_eq!(polled.listing, Err(GetMempoolListingError::Inactive));

    validator.latency(Duration::from_secs(2));
    let started = tokio::time::Instant::now();
    validator.get_block_links(&[h(0)]).await.expect("reachable");
    assert_eq!(started.elapsed(), Duration::from_secs(2), "one call = one latency");
    validator.latency(Duration::ZERO);
    validator.fail_next(1, FailureMode::Timeout);
    let failed = validator.get_block_links(&[h(0)]).await.expect_err("injected");
    assert_eq!(failed.mode, FailureMode::Timeout);
    assert!(validator.get_block_links(&[h(0), h(1)]).await.is_ok(), "one injected failure");
    validator.reachable(false);
    let gone = validator.get_poll_reading(false, &[]).await.expect_err("unreachable");
    assert_eq!(gone.mode, FailureMode::Connection);
    assert_eq!(validator.calls(), Calls { polls: 4, links: 4, blocks: 0, sends: 4 });
}
