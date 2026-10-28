//! Zaino's JSON-RPC client against a simulated zebrad: request params, envelopes, batches,
//! parsers and error classification, all on the real bytes

use zaino_primitives::types::TransactionLocation;
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

use super::*;
use crate::testing::{h, p2pkh, raw_transaction, raw_transparent};
use crate::{
    BlockLink, ChainDataSource, FailureMode, GetAtHeightError, GetBlockByHashError,
    GetTransactionError, MempoolListed, QueryError,
};

#[tokio::test]
async fn links_read_its_best_chain_and_a_height_above_its_tip_is_absent() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(1);
    chain.mine_empty(1);
    let client = chain.validator(tip).client();

    let links = client.get_block_links(&[h(0), h(1), h(2)]).await.expect("reachable");

    let link = |at: BlockRef| Ok(BlockLink { header: chain.header_bytes(at.hash) });
    let above = Err(GetAtHeightError::HeightNotFound(h(2)));
    assert_eq!(links, [link(chain.genesis()), link(tip), above], "links at 0, 1 (tip), 2");
}

/// `getblockhash` above the tip = zebrad's `-32602`: absent, never a failed poll
#[tokio::test]
async fn a_poll_reads_its_tip_and_a_height_above_it_is_absent_not_failed() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(1);
    chain.mine_empty(1);
    let client = chain.validator(tip).client();

    let polled = client.get_poll_reading(false, &[h(1), h(2)]).await.expect("reachable");

    assert_eq!(polled.info, chain.blockchain_info(tip), "getblockchaininfo at its tip");
    let [at_tip, above] = polled.held.as_slice() else {
        panic!("one answer per asked height (1, 2): {:?}", polled.held)
    };
    assert!(matches!(at_tip, Ok(hash) if *hash == tip.hash), "held at 1 (tip): {at_tip:?}");
    assert!(
        matches!(above, Err(QueryError::Domain(GetAtHeightError::HeightNotFound(at))) if *at == h(2)),
        "held at 2 (above the tip) = absent: {above:?}"
    );
}

/// Served bytes decode to the chain's own blocks
#[tokio::test]
async fn blocks_by_hash_or_height_are_the_chains_own() {
    let mut chain = MockChain::regtest();
    let one = chain.mine_empty(1);
    let tip = chain.mine_empty(1);
    let client = chain.validator(tip).client();

    let by_hash = client.get_block_by_hash(tip.hash).await.expect("on its best");
    let by_height = client.get_blocks_by_height(&[h(2), h(1)]).await.expect("on its best");

    assert_eq!(format!("{by_hash:?}"), format!("{:?}", chain.block(tip.hash)), "by hash");
    let hashes: Vec<BlockHash> = by_height.iter().map(|block| block.header().hash).collect();
    assert_eq!(hashes, [tip.hash, one.hash], "by height 2, 1: asked order");
}

/// Above its tip = absent (`-5` by hash, `-8` by height), one absent height fails the batch
#[tokio::test]
async fn a_block_above_its_tip_is_absent_by_hash_and_by_height() {
    let mut chain = MockChain::regtest();
    chain.mine_empty(1);
    let tip = chain.mine_empty(1);
    let above = chain.mine_empty(1);
    let client = chain.validator(tip).client();

    let unserved = client.get_block_by_hash(above.hash).await;
    let over = client.get_blocks_by_height(&[h(2), h(3)]).await;

    assert!(
        matches!(&unserved, Err(QueryError::Domain(GetBlockByHashError::NotFound(at))) if *at == above.hash),
        "by hash at 3 (above tip 2): {unserved:?}"
    );
    assert!(
        matches!(&over, Err(QueryError::Domain(GetAtHeightError::HeightNotFound(at))) if *at == h(3)),
        "by heights 2, 3 (above tip 2): {over:?}"
    );
}

/// Tip read before the reorg, `getblockhash` answered after it: that poll mixes both, the next whole
#[tokio::test]
async fn a_reorg_after_the_next_poll_splits_that_poll_only() {
    let mut chain = MockChain::regtest().varied_work();
    let one = chain.mine_empty(1);
    let tip = chain.mine_empty(1);
    let validator = chain.validator(tip);
    let client = validator.client();
    let fork = chain.fork(h(1)).outweigh().mine_empty(1).tip();

    validator.reorg_after_next_poll(&chain, fork);
    let raced = client.get_poll_reading(false, &[h(1), h(2)]).await.expect("reachable");
    let next = client.get_poll_reading(false, &[]).await.expect("reachable");

    assert_eq!(raced.info.tip, tip, "raced poll: tip read before the reorg");
    let [at_one, at_two] = raced.held.as_slice() else {
        panic!("one answer per asked height (1, 2): {:?}", raced.held)
    };
    assert!(matches!(at_one, Ok(hash) if *hash == one.hash), "raced held at 1: {at_one:?}");
    assert!(matches!(at_two, Ok(hash) if *hash == fork.hash), "raced held at 2 = fork: {at_two:?}");
    assert_eq!(next.info, chain.blockchain_info(fork), "next poll: the fork whole");
    let gone = client.get_block_by_hash(tip.hash).await;
    assert!(
        matches!(&gone, Err(QueryError::Domain(GetBlockByHashError::NotFound(at))) if *at == tip.hash),
        "reorged-out tip by hash: {gone:?}"
    );
}

/// Mined: its bytes + height; unknown: absent (`-5`)
#[tokio::test]
async fn a_mined_tx_is_served_by_its_bytes_and_height() {
    let mut chain = MockChain::regtest();
    let (txid, raw) = raw_transaction(7, 0);
    chain.mine(|b| b.raw_tx(raw.clone()));
    let client = chain.validator(chain.tip()).client();
    let never = TxId::from_bytes([0x33; 32]);

    let mined = client.get_transaction(txid).await.expect("mined on its best");
    let unknown = client.get_transaction(never).await;

    assert_eq!(mined.bytes, raw, "{txid}: its bytes");
    assert_eq!(mined.location, TransactionLocation::BestChain(h(1)), "{txid}: mined at 1");
    assert!(
        matches!(&unknown, Err(QueryError::Domain(GetTransactionError::NotFound(_)))),
        "{never}: {unknown:?}"
    );
}

/// Alice's 5 000 coinbases at 1 and 2: one spend heard (fee 2 000), one relayed (fee 1 000)
#[tokio::test]
async fn mempool_lists_heard_and_relayed_txs_at_the_fee_their_spends_leave() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let one = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let tip = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let validator = chain.validator(tip);
    let client = validator.client();
    let (heard, heard_raw) = raw_transparent(&[chain.output(tip, 0, 0)], &[(&alice, 3_000)]);
    let (sent, sent_raw) = raw_transparent(&[chain.output(one, 0, 0)], &[(&alice, 4_000)]);
    chain.submit_raw(heard_raw.clone());
    validator.hear(&chain, heard);

    let relayed = client.send_raw_transaction(sent_raw.clone()).await;
    let listed = client.get_poll_reading(false, &[]).await.expect("reachable").listing;

    assert!(matches!(&relayed, Ok(txid) if *txid == sent), "relayed {sent}: {relayed:?}");
    let entry = |txid, zats, raw: &[u8]| MempoolListed {
        txid,
        fee: Zatoshis::from_u64(zats).expect("in supply"),
        encoded_len: u32::try_from(raw.len()).expect("a small transaction"),
    };
    let mut expected = vec![entry(heard, 2_000, &heard_raw), entry(sent, 1_000, &sent_raw)];
    expected.sort_by_key(|entry| entry.txid);
    let mut listed = listed.expect("listed");
    listed.sort_by_key(|entry| entry.txid);
    assert_eq!(listed, expected, "heard + relayed, by txid");
}

#[tokio::test]
async fn a_listed_tx_is_served_by_its_bytes() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let tip = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let validator = chain.validator(tip);
    let client = validator.client();
    let (sent, sent_raw) = raw_transparent(&[chain.output(tip, 0, 0)], &[(&alice, 4_000)]);
    client.send_raw_transaction(sent_raw.clone()).await.expect("relayed");
    let listed = MempoolListed {
        txid: sent,
        fee: Zatoshis::from_u64(1_000).expect("in supply"),
        encoded_len: u32::try_from(sent_raw.len()).expect("a small transaction"),
    };

    let bytes = client.get_raw_mempool_transactions(&[listed]).await.expect("reachable");

    assert_eq!(bytes, [Ok(sent_raw)], "{sent}: its relayed bytes");
}

#[tokio::test]
async fn a_mined_or_evicted_tx_is_unlisted() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let one = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let tip = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let validator = chain.validator(tip);
    let client = validator.client();
    let (mined, mined_raw) = raw_transparent(&[chain.output(one, 0, 0)], &[(&alice, 4_000)]);
    let (evicted, evicted_raw) = raw_transparent(&[chain.output(tip, 0, 0)], &[(&alice, 3_000)]);
    chain.submit_raw(mined_raw.clone());
    chain.submit_raw(evicted_raw);
    validator.hear(&chain, mined);
    validator.hear(&chain, evicted);

    let three = chain.mine(|b| b.raw_tx(mined_raw));
    validator.follow(&chain, three);
    validator.evict(evicted);

    let listed = client.get_poll_reading(false, &[]).await.expect("reachable").listing;
    assert_eq!(listed, Ok(Vec::new()), "{mined} mined at 3, {evicted} evicted: neither listed");
}

#[tokio::test]
async fn a_check_judges_a_tx_and_lists_nothing() {
    let alice = p2pkh([0xaa; 20]);
    let mut chain = MockChain::regtest();
    let tip = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 5_000)));
    let client = chain.validator(tip).client();
    let (spend, spend_raw) = raw_transparent(&[chain.output(tip, 0, 0)], &[(&alice, 4_000)]);

    let checked = client.check_admission(spend_raw).await;

    assert!(matches!(&checked, Ok(txid) if *txid == spend), "admissible {spend}: {checked:?}");
    let listed = client.get_poll_reading(false, &[]).await.expect("reachable").listing;
    assert_eq!(listed, Ok(Vec::new()), "a check lists nothing");
}

/// Unknown prevout = `-25` (consensus-invalid on its tip), garbage bytes = `-22` (malformed)
#[tokio::test]
async fn a_send_is_refused_with_zebrads_code_for_why() {
    let alice = p2pkh([0xaa; 20]);
    let chain = MockChain::regtest();
    let client = chain.validator(chain.tip()).client();
    let (_, unknown_raw) = raw_transparent(&[OutPoint::new([0x09; 32], 0)], &[(&alice, 4_000)]);

    let unknown = client.send_raw_transaction(unknown_raw).await;
    let garbage = client.send_raw_transaction(vec![9; 8]).await;

    assert!(
        matches!(&unknown, Err(QueryError::Domain(SendRawTransactionError::Rejected { code: -25, message })) if message.starts_with("spends ")),
        "spend of an unknown prevout: {unknown:?}"
    );
    assert!(
        matches!(&garbage, Err(QueryError::Domain(SendRawTransactionError::Malformed(_)))),
        "8 bytes of 0x09: {garbage:?}"
    );
}

/// `-32601` (zcashd, stock zebrad) = unsupported, never a failure
#[tokio::test]
async fn a_node_lacking_checkadmission_reads_as_unsupported() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    let client = validator.client();

    validator.lacks(&["checkadmission"]);
    let unsupported = client.check_admission(raw_transaction(3, 0).1).await;

    assert!(matches!(unsupported, Err(QueryError::Unsupported)), "{unsupported:?}");
}

/// Metadata rides the poll batch: a failing `getpeerinfo` fails that item alone
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

    assert_eq!(polled.info.estimated_height, h(100), "scripted estimate");
    let metadata = polled.metadata.expect("asked");
    assert_eq!(metadata.answered, Telemetry { peers: None, release: Some(release) }, "answered");
    let failed: Vec<_> = metadata.failed.iter().map(|(what, cause)| (*what, cause.mode)).collect();
    assert_eq!(failed, [("peer list", FailureMode::RpcError(-1))], "(failed read, mode)");
}

/// No `getdeprecationinfo` (zebrad < 6.3) = end of service unknown
#[tokio::test]
async fn a_node_lacking_getdeprecationinfo_reads_as_end_of_service_unknown() {
    let chain = MockChain::regtest();
    let validator = chain.validator(chain.tip());
    let client = validator.client();

    validator.lacks(&["getdeprecationinfo"]);
    let polled = client.get_poll_reading(true, &[]).await.expect("reachable");

    let release = polled.metadata.expect("asked").answered.release.expect("getinfo answered");
    assert_eq!(release.end_of_service, EndOfService::Unknown, "{release:?}");
}

const HEADER_LATENCY: Duration = Duration::from_secs(2);

/// Latency delays only requests carrying a slow method (paused clock: exact elapsed)
#[tokio::test(start_paused = true)]
async fn a_slow_method_delays_only_requests_carrying_it() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(2);
    let validator = chain.validator(tip);
    let client = validator.client();
    validator.latency(&["getblockheader"], HEADER_LATENCY);

    let started = tokio::time::Instant::now();
    client.get_block_links(&[h(0), h(1), h(2)]).await.expect("reachable");
    let links_done = started.elapsed();
    client.get_poll_reading(false, &[]).await.expect("reachable");
    let poll_took = started.elapsed() - links_done;

    assert_eq!(links_done, HEADER_LATENCY, "one getblockheader batch: one delay");
    assert_eq!(poll_took, Duration::ZERO, "poll carries no getblockheader");
}

/// - HTTP 503: requests carrying an unreachable method, every request once `down`
/// - `calls` counts each answered batch item, nothing of a refused request
#[tokio::test]
async fn refused_requests_answer_503_and_calls_count_each_answered_item() {
    let mut chain = MockChain::regtest();
    let tip = chain.mine_empty(2);
    let validator = chain.validator(tip);
    let client = validator.client();
    validator.reachable(&["sendrawtransaction"], false);

    client.get_block_links(&[h(0), h(1), h(2)]).await.expect("reachable");
    let unreachable = client.send_raw_transaction(raw_transaction(4, 0).1).await;
    validator.down(true);
    let down = client.get_poll_reading(false, &[]).await;

    let unreachable = unreachable.map_err(|error| match error {
        QueryError::NonDomain(cause) => Some(cause.mode),
        _ => None,
    });
    let refused = Some(FailureMode::HttpStatus(503));
    assert_eq!(unreachable, Err(refused), "send with sendrawtransaction unreachable");
    let down = down.map(|_| ()).map_err(|cause| Some(cause.mode));
    assert_eq!(down, Err(refused), "poll on a down node");
    assert_eq!(validator.calls("getblockheader"), 3, "three headers in one batch");
    assert_eq!(validator.calls("sendrawtransaction"), 0, "unreachable: answered nothing");
    assert_eq!(validator.calls("getblockchaininfo"), 0, "down: answered nothing");
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
