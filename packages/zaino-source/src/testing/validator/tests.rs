//! Zaino's JSON-RPC client against a simulated zebrad: request params, envelopes, batches,
//! parsers and error classification, all on the real bytes

use zaino_primitives::sha256d;
use zaino_primitives::types::{MerkleRoot, TransactionLocation};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

use super::*;
use crate::testing::{all_at, h, p2pkh, raw_transaction, raw_transparent};
use crate::{
    BlockLink, ChainDataSource, FailureMode, GetAtHeightError, GetBlockByHashError, MempoolListed,
    QueryError,
};

#[tokio::test]
async fn links_and_polls_read_its_best_chain_up_to_its_tip() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(1);
    chain.mine_empty(1);
    let client = chain.validator(tip).client();

    let links = client.get_block_links(&[h(0), h(1), h(2)]).await.expect("reachable");
    let polled = client.get_poll_reading(false, &[h(1), h(2)]).await.expect("reachable");

    let link = |at: BlockRef| Ok(BlockLink { header: chain.header_bytes(at.hash) });
    let above = Err(GetAtHeightError::HeightNotFound(h(2)));
    assert_eq!(links, [link(chain.genesis()), link(tip), above]);
    assert_eq!(polled.info, chain.blockchain_info(tip));
    let held: Vec<Option<BlockHash>> =
        polled.held.iter().map(|held| held.as_ref().ok().copied()).collect();
    assert_eq!(held, [Some(tip.hash), None], "getblockhash above the tip = absent (-32602)");
}

/// - Tip + estimate at the asked block
/// - Every scheduled upgrade listed (NU7 pending included) → listing reads back as the chain's
///   own schedule
#[test]
fn blockchain_info_reports_the_tip_and_reads_back_as_the_chains_schedule() {
    let early = Some(h(1));
    let upgrades =
        LocalNetwork { overwinter: early, sapling: early, nu7: Some(h(5)), ..all_at(h(3)) };
    let mut chain = MockChain::regtest().upgrades(upgrades);
    chain.mine_empty(6);

    let info = chain.blockchain_info(chain.at(h(4)));
    assert_eq!((info.tip, info.estimated_height), (chain.at(h(4)), h(4)));
    assert_eq!(info.activations.len(), 11, "every scheduled upgrade, NU7 included");
    assert_eq!(info.schedule(), Ok(chain.schedule().upgrades));
}

/// - Served bytes decode to the chain's own blocks
/// - Above its tip = absent (`-5` by hash, `-8` by height)
/// - One absent height fails the batch
#[tokio::test]
async fn blocks_by_hash_or_height_come_from_its_best_chain_only() {
    let mut chain = MockChain::regtest();
    chain.mine_empty(1);
    let tip = chain.mine_empty(1);
    let above = chain.mine_empty(1);
    let client = chain.validator(tip).client();

    let by_hash = client.get_block_by_hash(tip.hash).await.expect("on its best");
    let by_height = client.get_blocks_by_height(&[h(2), h(1)]).await.expect("on its best");
    let hashes: Vec<_> = by_height.iter().map(|block| block.header().hash).collect();
    assert_eq!((by_hash.header().hash, hashes), (tip.hash, vec![tip.hash, chain.at(h(1)).hash]));
    assert_eq!(format!("{by_hash:?}"), format!("{:?}", chain.block(tip.hash)), "the chain's block");

    let unserved = client.get_block_by_hash(above.hash).await;
    let over = client.get_blocks_by_height(&[h(2), h(3)]).await;
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
    let client = validator.client();
    let fork = chain.fork(h(1)).outweigh().mine_empty(1).tip();

    validator.reorg_after_next_poll(&chain, fork);
    let raced = client.get_poll_reading(false, &[h(1), h(2)]).await.expect("reachable");
    let next = client.get_poll_reading(false, &[]).await.expect("reachable");

    let held: Vec<Option<BlockHash>> =
        raced.held.iter().map(|held| held.as_ref().ok().copied()).collect();
    let after = vec![Some(chain.at(h(1)).hash), Some(fork.hash)];
    assert_eq!((raced.info.tip.hash, held), (tip.hash, after));
    assert_eq!(next.info, chain.blockchain_info(fork));
    let gone = client.get_block_by_hash(tip.hash).await;
    assert!(matches!(gone, Err(QueryError::Domain(GetBlockByHashError::NotFound(_)))));
}

/// - Mined: its bytes + height
/// - Unknown: absent (`-5`)
#[tokio::test]
async fn a_mined_tx_is_served_by_its_bytes_and_height() {
    let mut chain = MockChain::regtest();
    let (txid, raw) = raw_transaction(7, 0);
    let tip = chain.mine(|b| b.raw_tx(raw.clone()));
    let client = chain.validator(tip).client();

    let mined = client.get_transaction(txid).await.expect("mined on its best");
    let unknown = client.get_transaction(TxId::from_bytes([0x33; 32])).await;

    assert_eq!((mined.bytes, mined.location), (raw, TransactionLocation::BestChain(h(1))));
    assert!(matches!(unknown, Err(QueryError::Domain(crate::GetTransactionError::NotFound(_)))));
}

/// `(asked hash, header hashes itself, txids rebuild its merkle root, height)` per lie, by hash
/// and by height, as Zaino decodes the served bytes
#[tokio::test]
async fn each_lie_breaks_exactly_what_it_names() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine(|b| b.raw_tx(raw_transaction(7, 0).1).tx(|t| t.sapling_output(1)));
    let validator = chain.validator(tip);
    let client = validator.client();
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
        let by_hash = client.get_block_by_hash(tip.hash).await.expect("answers");
        let by_height = client.get_blocks_by_height(&[h(1)]).await.expect("answers").remove(0);
        assert_eq!((shape(&by_hash), shape(&by_height)), (expected, expected), "{lie:?}");
    }
}

#[tokio::test]
async fn a_tampered_header_is_served_and_hashed_as_edited() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(1);
    let validator = chain.validator(tip);
    let client = validator.client();
    let early = chain.block(chain.genesis().hash).header().time;

    validator.tamper(h(1), |header| header.time = early);
    let held = client.get_poll_reading(false, &[h(1)]).await.expect("reachable").held;
    let tampered = *held[0].as_ref().expect("held");
    let link = client.get_block_links(&[h(1)]).await.expect("reachable").remove(0);
    let block = client.get_block_by_hash(tampered).await.expect("served by its new hash");

    assert_ne!(tampered, tip.hash, "edited header, rehashed");
    let linked = BlockHash(sha256d(&link.expect("held").header));
    assert_eq!((linked, block.header().time), (tampered, early));
}

/// Two 5 000 coinbases to Alice → heard + relayed spends listed at the fee their spends leave
/// (zebrad's `f64` ZEC fee, read back exact)
#[tokio::test]
async fn mempool_lists_heard_and_relayed_txs_until_mined_or_evicted() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let one = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let tip = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let validator = chain.validator(tip);
    let client = validator.client();
    let (heard, heard_raw) = raw_transparent(&[chain.output(tip, 0, 0)], &[(&alice, 3_000)]);
    let (sent, sent_raw) = raw_transparent(&[chain.output(one, 0, 0)], &[(&alice, 4_000)]);
    let entry = |txid, zats, raw: &[u8]| MempoolListed {
        txid,
        fee: Zatoshis::from_u64(zats).expect("in supply"),
        encoded_len: u32::try_from(raw.len()).expect("a small transaction"),
    };
    let heard_entry = entry(heard, 2_000, &heard_raw);
    let sent_entry = entry(sent, 1_000, &sent_raw);

    chain.submit_raw(heard_raw);
    validator.hear(&chain, heard);
    assert_eq!(client.send_raw_transaction(sent_raw.clone()).await.expect("relayed"), sent);
    let listed = client.get_poll_reading(false, &[]).await.expect("reachable").listing;
    let bytes = client.get_raw_mempool_transactions(&[sent_entry]).await.expect("reachable");
    let mut both = vec![heard_entry, sent_entry];
    both.sort_by_key(|entry| entry.txid);
    let mut listed = listed.expect("listed");
    listed.sort_by_key(|entry| entry.txid);
    assert_eq!((listed, bytes), (both, vec![Ok(sent_raw.clone())]));

    let mined = chain.mine(|b| b.raw_tx(sent_raw));
    validator.follow(&chain, mined);
    let listed = client.get_poll_reading(false, &[]).await.expect("reachable").listing;
    assert_eq!(listed, Ok(vec![heard_entry]), "mined = unlisted");

    validator.evict(heard);
    let listed = client.get_poll_reading(false, &[]).await.expect("reachable").listing;
    assert_eq!(listed, Ok(Vec::new()), "evicted = unlisted");
}

/// - Check = verdict only, nothing listed
/// - Unknown prevout (`-25`) / garbage bytes (`-22`) = refused, as zebrad's codes classify
#[tokio::test]
async fn sends_and_checks_judge_a_tx_on_its_tip() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let tip = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let client = chain.validator(tip).client();
    let (spend, spend_raw) = raw_transparent(&[chain.output(tip, 0, 0)], &[(&alice, 4_000)]);
    let (_, unknown_raw) = raw_transparent(&[OutPoint::new([0x09; 32], 0)], &[(&alice, 4_000)]);

    assert_eq!(client.check_admission(spend_raw).await.expect("admissible"), spend);
    let listed = client.get_poll_reading(false, &[]).await.expect("reachable").listing;
    assert_eq!(listed, Ok(Vec::new()), "a check lists nothing");
    let unknown = client.send_raw_transaction(unknown_raw).await;
    assert!(
        matches!(&unknown, Err(QueryError::Domain(SendRawTransactionError::Rejected { code: -25, message })) if message.starts_with("spends ")),
        "{unknown:?}"
    );
    let garbage = client.send_raw_transaction(vec![9; 8]).await;
    assert!(matches!(garbage, Err(QueryError::Domain(SendRawTransactionError::Malformed(_)))));
}

/// - A scripted refusal answers sends and checks alike
/// - A node lacking `checkadmission` (`-32601`) = unsupported, never a failure
#[tokio::test]
async fn a_scripted_refusal_answers_sends_and_checks_and_a_missing_method_is_unsupported() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    let client = validator.client();
    let fee_too_low =
        SendRawTransactionError::Rejected { code: -26, message: "fee too low".into() };

    validator.relay(Err(fee_too_low.clone()));
    let sent = client.send_raw_transaction(raw_transaction(3, 0).1).await;
    let checked = client.check_admission(raw_transaction(3, 0).1).await;
    validator.lacks(&["checkadmission"]);
    let unsupported = client.check_admission(raw_transaction(3, 0).1).await;

    assert!(matches!(&sent, Err(QueryError::Domain(r)) if *r == fee_too_low), "{sent:?}");
    assert!(matches!(&checked, Err(QueryError::Domain(r)) if *r == fee_too_low), "{checked:?}");
    assert!(matches!(unsupported, Err(QueryError::Unsupported)), "{unsupported:?}");
}

/// Metadata rides the poll batch
///
/// - A failing `getpeerinfo` fails that item alone
/// - A node without `getdeprecationinfo` = end of service unknown
#[tokio::test]
async fn poll_metadata_and_estimate_answer_as_scripted() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    let client = validator.client();
    let release = NodeRelease {
        build: "v6.4.2".to_owned(),
        user_agent: "/Zebra:6.4.2/".to_owned(),
        end_of_service: EndOfService::At { height: h(3_564_960) },
    };

    validator.telemetry(Telemetry { peers: None, release: Some(release.clone()) });
    validator.estimate(h(100));
    let polled = client.get_poll_reading(true, &[]).await.expect("reachable");
    validator.lacks(&["getdeprecationinfo"]);
    let older = client.get_poll_reading(true, &[]).await.expect("reachable");

    let metadata = polled.metadata.expect("asked");
    assert_eq!(polled.info.estimated_height, h(100));
    assert_eq!(metadata.answered, Telemetry { peers: None, release: Some(release) });
    let failed: Vec<_> = metadata.failed.iter().map(|(what, cause)| (*what, cause.mode)).collect();
    assert_eq!(failed, [("peer list", FailureMode::RpcError(-1))]);
    let older = older.metadata.expect("asked").answered.release.expect("answered");
    assert_eq!(older.end_of_service, EndOfService::Unknown);
}

/// - `-1` "mempool is not active" = inactive
/// - `-32601` = no mempool at all
#[tokio::test]
async fn a_scripted_listing_refusal_replaces_the_listing() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    let client = validator.client();

    validator.listing(Err(GetMempoolListingError::Inactive));
    let inactive = client.get_poll_reading(false, &[]).await.expect("reachable").listing;
    validator.listing(Err(GetMempoolListingError::Unavailable));
    let unavailable = client.get_poll_reading(false, &[]).await.expect("reachable").listing;

    assert_eq!(inactive, Err(GetMempoolListingError::Inactive));
    assert_eq!(unavailable, Err(GetMempoolListingError::Unavailable));
}

/// Faults by wire method
///
/// - A slow one delays only requests carrying it
/// - An unreachable one refuses those (HTTP 503), the rest answered
/// - `calls` counts each batch item
#[tokio::test(start_paused = true)]
async fn faults_apply_per_wire_method_and_calls_count_each_item() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(2);
    let validator = chain.validator(tip);
    let client = validator.client();
    validator.latency(&["getblockheader"], Duration::from_secs(2));
    validator.reachable(&["sendrawtransaction"], false);

    let started = tokio::time::Instant::now();
    client.get_block_links(&[h(0), h(1), h(2)]).await.expect("reachable");
    let links_done = started.elapsed();
    client.get_poll_reading(false, &[]).await.expect("reachable");
    let refused = client.send_raw_transaction(raw_transaction(4, 0).1).await;

    let two = Duration::from_secs(2);
    assert_eq!(
        (links_done, started.elapsed()),
        (two, two),
        "one links batch delayed, the poll at once"
    );
    assert!(
        matches!(&refused, Err(QueryError::NonDomain(cause)) if cause.mode == FailureMode::HttpStatus(503)),
        "{refused:?}"
    );
    let calls =
        ["getblockheader", "getblockchaininfo", "sendrawtransaction"].map(|m| validator.calls(m));
    assert_eq!(calls, [3, 1, 0], "three headers in one batch; a refused request answers nothing");

    validator.down(true);
    let down = client.get_poll_reading(false, &[]).await;
    assert!(matches!(down, Err(cause) if cause.mode == FailureMode::HttpStatus(503)));
}

#[test]
#[should_panic(expected = "not in the chain's mempool")]
fn hearing_a_transaction_the_chain_never_held_panics() {
    let chain = MockChain::regtest();
    chain.validator(chain.tip()).hear(&chain, raw_transaction(1, 0).0);
}

#[test]
#[should_panic(expected = "getblokc: not a method MockValidator serves")]
fn scripting_a_method_it_does_not_serve_panics() {
    let chain = MockChain::regtest();
    chain.validator(chain.tip()).latency(&["getblokc"], Duration::from_secs(1));
}
