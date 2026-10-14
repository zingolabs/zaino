use zaino_primitives::sha256d;
use zaino_primitives::testing::{h, outpoint, p2pkh};
use zaino_primitives::types::MerkleRoot;
use zcash_protocol::value::Zatoshis;

use super::*;

#[tokio::test]
async fn links_and_polls_read_its_best_chain_up_to_its_tip() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(1);
    chain.mine_empty(1);
    let validator = chain.validator(tip);

    let links = validator.get_block_links(&[h(0), h(1), h(2)]).await.expect("reachable");
    let polled = validator.get_poll_reading(false, &[]).await.expect("reachable");

    let link = |at: BlockRef| Ok(BlockLink { header: chain.header_bytes(at.hash) });
    let above = Err(GetAtHeightError::HeightNotFound(h(2)));
    assert_eq!(links, [link(chain.genesis()), link(tip), above]);
    assert_eq!(polled.info, chain.blockchain_info(tip));
}

#[tokio::test]
async fn blocks_by_hash_or_height_come_from_its_best_chain_only() {
    let mut chain = MockChain::regtest();
    chain.mine_empty(1);
    let tip = chain.mine_empty(1);
    let above = chain.mine_empty(1);
    let validator = chain.validator(tip);

    let by_hash = validator.get_block_by_hash(tip.hash).await.expect("on its best");
    let by_height = validator.get_blocks_by_height(&[h(2), h(1)]).await.expect("on its best");
    let hashes: Vec<_> = by_height.iter().map(|block| block.header().hash).collect();
    assert_eq!((by_hash.header().hash, hashes), (tip.hash, vec![tip.hash, chain.at(h(1)).hash]));

    let unserved = validator.get_block_by_hash(above.hash).await;
    let over = validator.get_blocks_by_height(&[h(2), h(3)]).await;
    assert!(
        matches!(&unserved, Err(QueryError::Domain(GetBlockByHashError::NotFound(at))) if *at == above.hash),
        "{unserved:?}"
    );
    assert!(
        matches!(&over, Err(QueryError::Domain(GetAtHeightError::HeightNotFound(at))) if *at == h(3)),
        "one height above its tip fails the batch: {over:?}"
    );
}

/// Tip read before the reorg, `getblockhash` answered after it: that poll mixes both, the next whole
#[tokio::test]
async fn a_reorg_after_the_next_poll_splits_that_poll_only() {
    let mut chain = MockChain::regtest().varied_work();
    chain.mine_empty(1);
    let tip = chain.mine_empty(1);
    let validator = chain.validator(tip);
    let fork = chain.fork(h(1)).outweigh().mine_empty(1).tip();

    validator.reorg_after_next_poll(&chain, fork);
    let raced = validator.get_poll_reading(false, &[h(1), h(2)]).await.expect("reachable");
    let next = validator.get_poll_reading(false, &[]).await.expect("reachable");

    let held: Vec<Option<BlockHash>> =
        raced.held.iter().map(|held| held.as_ref().ok().copied()).collect();
    let after = vec![Some(chain.at(h(1)).hash), Some(fork.hash)];
    assert_eq!((raced.info.best_block_hash, held), (tip.hash, after));
    assert_eq!(next.info, chain.blockchain_info(fork));
    let gone = validator.get_block_by_hash(tip.hash).await;
    assert!(matches!(gone, Err(QueryError::Domain(GetBlockByHashError::NotFound(_)))));
}

#[tokio::test]
async fn a_mined_raw_tx_is_served_by_its_bytes_and_height() {
    let mut chain = MockChain::regtest();
    let (txid, raw) = raw_transaction(7, 0);
    let tip = chain.mine(|b| b.raw_tx(decoded(raw.clone())));

    let mined = chain.validator(tip).get_transaction(txid).await.expect("mined on its best");

    assert_eq!((mined.bytes, mined.location), (raw, TransactionLocation::BestChain(h(1))));
}

#[tokio::test]
#[should_panic(expected = "mined without bytes")]
async fn a_tx_builder_tx_has_no_body_to_serve() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine(|b| b.tx(|t| t.txid([0x33; 32])));
    let _ = chain.validator(tip).get_transaction(TransactionId::from([0x33; 32])).await;
}

/// `(asked hash, header hashes itself, txids rebuild its merkle root, height)` per lie
#[tokio::test]
async fn each_lie_breaks_exactly_what_it_names() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine(|b| b.raw_tx(decoded(raw_transaction(7, 0).1)).tx(|t| t.txid([0x33; 32])));
    let validator = chain.validator(tip);
    let shape = |block: &Block| {
        let header = block.header();
        let txids: Vec<_> = block.transactions().iter().map(|tx| tx.txid).collect();
        let rebuilds = MerkleRoot::of_txids(&txids) == Some(header.merkle_root);
        (header.hash == tip.hash, header_hash(header) == header.hash, rebuilds, header.height)
    };
    #[rustfmt::skip]
    let lies = [
        (None,                    (true,  true, true,  h(1))),
        (Some(Lie::WrongBlock),   (false, true, true,  h(1))),
        (Some(Lie::Poisoned),     (true,  true, false, h(1))),
        (Some(Lie::Mutated),      (true,  true, false, h(1))),
        (Some(Lie::WrongHeight),  (true,  true, true,  h(2))),
    ];
    for (lie, expected) in lies {
        validator.lie(lie);
        let by_hash = validator.get_block_by_hash(tip.hash).await.expect("answers");
        let by_height = validator.get_blocks_by_height(&[h(1)]).await.expect("answers").remove(0);
        assert_eq!((shape(&by_hash), shape(&by_height)), (expected, expected), "{lie:?}");
    }
}

#[tokio::test]
async fn a_tampered_header_is_served_and_hashed_as_edited() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(1);
    let validator = chain.validator(tip);
    let early = chain.block(chain.genesis().hash).header().time;

    validator.tamper(h(1), |header| header.time = early);
    let held = validator.get_poll_reading(false, &[h(1)]).await.expect("reachable").held;
    let tampered = *held[0].as_ref().expect("held");
    let link = validator.get_block_links(&[h(1)]).await.expect("reachable").remove(0);
    let block = validator.get_block_by_hash(tampered).await.expect("served by its new hash");

    assert_ne!(tampered, tip.hash, "edited header, rehashed");
    let linked = BlockHash::from(sha256d(&link.expect("held").header));
    assert_eq!((linked, block.header().time), (tampered, early));
}

/// Two 5 000 coinbases to Alice; heard + relayed spends listed at the fee their spends leave
#[tokio::test]
async fn mempool_lists_heard_and_relayed_txs_until_mined_or_evicted() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    chain.mine(|b| b.coinbase(|c| c.txid([0x01; 32]).pay(&alice, 5_000)));
    let tip = chain.mine(|b| b.coinbase(|c| c.txid([0x02; 32]).pay(&alice, 5_000)));
    let validator = chain.validator(tip);
    let (heard, heard_raw) = raw_transparent(&[outpoint([0x02; 32], 0)], &[(&alice, 3_000)]);
    let (sent, sent_raw) = raw_transparent(&[outpoint([0x01; 32], 0)], &[(&alice, 4_000)]);
    let entry = |txid, zats, raw: &[u8]| MempoolListed {
        txid,
        fee: Zatoshis::from_u64(zats).expect("in supply"),
        encoded_len: u32::try_from(raw.len()).expect("a small transaction"),
    };
    let heard_entry = entry(heard, 2_000, &heard_raw);
    let sent_entry = entry(sent, 1_000, &sent_raw);

    chain.submit(decoded(heard_raw));
    validator.hear(&chain, heard);
    assert_eq!(validator.send_raw_transaction(sent_raw.clone()).await.expect("relayed"), sent);
    let listed = validator.get_poll_reading(false, &[]).await.expect("reachable").listing;
    let bytes = validator.get_raw_mempool_transactions(&[sent_entry]).await.expect("reachable");
    let mut both = vec![heard_entry, sent_entry];
    both.sort_by_key(|entry| entry.txid);
    assert_eq!((listed, bytes), (Ok(both), vec![Ok(sent_raw.clone())]));

    let mined = chain.mine(|b| b.raw_tx(decoded(sent_raw)));
    validator.follow(&chain, mined);
    let listed = validator.get_poll_reading(false, &[]).await.expect("reachable").listing;
    assert_eq!(listed, Ok(vec![heard_entry]), "mined = unlisted");

    validator.evict(heard);
    let listed = validator.get_poll_reading(false, &[]).await.expect("reachable").listing;
    assert_eq!(listed, Ok(Vec::new()), "evicted = unlisted");
}

/// Check = verdict only, nothing listed; unknown prevout / garbage bytes = refused
#[tokio::test]
async fn sends_and_checks_judge_a_tx_on_its_tip() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let tip = chain.mine(|b| b.coinbase(|c| c.txid([0x01; 32]).pay(&alice, 5_000)));
    let validator = chain.validator(tip);
    let (spend, spend_raw) = raw_transparent(&[outpoint([0x01; 32], 0)], &[(&alice, 4_000)]);
    let (_, unknown_raw) = raw_transparent(&[outpoint([0x09; 32], 0)], &[(&alice, 4_000)]);

    assert_eq!(validator.check_admission(spend_raw).await.expect("admissible"), spend);
    let listed = validator.get_poll_reading(false, &[]).await.expect("reachable").listing;
    assert_eq!(listed, Ok(Vec::new()), "a check lists nothing");
    let unknown = validator.send_raw_transaction(unknown_raw).await;
    assert!(
        matches!(&unknown, Err(QueryError::Domain(SendRawTransactionError::Rejected { code: -25, message })) if message.starts_with("spends ")),
        "{unknown:?}"
    );
    let garbage = validator.send_raw_transaction(vec![9; 8]).await;
    assert!(matches!(garbage, Err(QueryError::Domain(SendRawTransactionError::Malformed(_)))));
}

#[tokio::test]
async fn a_scripted_refusal_answers_sends_and_checks_alike() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    let fee_too_low =
        SendRawTransactionError::Rejected { code: -25, message: "fee too low".into() };

    validator.relay(Err(fee_too_low.clone()));
    let sent = validator.send_raw_transaction(raw_transaction(3, 0).1).await;
    let checked = validator.check_admission(raw_transaction(3, 0).1).await;

    assert!(matches!(&sent, Err(QueryError::Domain(r)) if *r == fee_too_low), "{sent:?}");
    assert!(
        matches!(&checked, Err(QueryError::Domain(CheckAdmissionError::Refused(r))) if *r == fee_too_low),
        "{checked:?}"
    );
}

#[tokio::test]
async fn a_validator_without_checkadmission_answers_unsupported() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());

    validator.checks_admission(false);
    let unsupported = validator.check_admission(raw_transaction(3, 0).1).await;

    assert!(matches!(unsupported, Err(QueryError::Domain(CheckAdmissionError::Unsupported))));
}

#[tokio::test]
async fn poll_metadata_and_estimate_answer_as_scripted() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    let release = NodeRelease {
        build: "v6.4.2".to_owned(),
        user_agent: "/Zebra:6.4.2/".to_owned(),
        protocol_version: 170_140,
        end_of_service: EndOfService::NotEnforced,
    };

    validator.metadata(None, Some(release.clone()));
    validator.estimate(h(100));
    let polled = validator.get_poll_reading(true, &[]).await.expect("reachable");

    let metadata = polled.metadata.expect("asked");
    assert_eq!(polled.info.estimated_height, h(100));
    assert_eq!(metadata.peers.expect_err("times out").mode, FailureMode::Timeout);
    assert_eq!(metadata.release.expect("answered"), release);
}

#[tokio::test]
async fn a_scripted_listing_refusal_replaces_the_listing() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());

    validator.listing(Err(GetMempoolListingError::Inactive));
    let polled = validator.get_poll_reading(false, &[]).await.expect("reachable");

    assert_eq!(polled.listing, Err(GetMempoolListingError::Inactive));
}

#[tokio::test(start_paused = true)]
async fn latency_delays_each_call_on_its_ports_only() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    validator.latency(&[Port::Links], Duration::from_secs(2));

    let started = tokio::time::Instant::now();
    validator.get_block_links(&[h(0)]).await.expect("reachable");
    let links_done = started.elapsed();
    validator.get_poll_reading(false, &[]).await.expect("reachable");
    let poll_done = started.elapsed();

    let two = Duration::from_secs(2);
    assert_eq!((links_done, poll_done), (two, two), "one links call delayed, the poll at once");
    assert_eq!(validator.calls(), Calls { polls: 1, links: 1, ..Calls::default() });
}

#[tokio::test]
async fn fail_next_refuses_that_many_calls_on_any_port_then_answers() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());

    validator.fail_next(1, FailureMode::Timeout);
    let failed = validator.get_block_links(&[h(0)]).await.expect_err("injected");
    let next = validator.get_poll_reading(false, &[]).await;

    assert_eq!((failed.mode, next.is_ok()), (FailureMode::Timeout, true));
    assert_eq!(
        validator.calls(),
        Calls { polls: 1, links: 1, ..Calls::default() },
        "refused counted"
    );
}

#[tokio::test]
async fn an_unreachable_port_refuses_in_transport_until_set_back() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    let (txid, raw) = raw_transaction(4, 0);

    validator.reachable(&[Port::Send], false);
    let refused = validator.send_raw_transaction(raw.clone()).await;
    let polled = validator.get_poll_reading(false, &[]).await;
    validator.reachable(&[Port::Send], true);
    let back = validator.send_raw_transaction(raw).await;

    assert!(
        matches!(&refused, Err(QueryError::NonDomain(cause)) if cause.mode == FailureMode::Connection),
        "{refused:?}"
    );
    assert!(polled.is_ok(), "other ports reachable");
    assert_eq!(back.expect("reachable again"), txid);
    assert_eq!(validator.calls(), Calls { polls: 1, sends: 2, ..Calls::default() });
}

#[test]
#[should_panic(expected = "not in the chain's mempool")]
fn hearing_a_transaction_the_chain_never_held_panics() {
    let chain = MockChain::regtest();
    chain.validator(chain.tip()).hear(&chain, raw_transaction(1, 0).0);
}
