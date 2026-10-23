//! Zaino's peer set against a scripted zebra-network peer on loopback (real handshakes, real
//! codec): no DNS, no internet
//!
//! - multi_thread on every test: zebra-network's codec parses blocks and txs in `block_in_place`
//! - real clock (zebra's own timers and sockets), every wait bounded by `within`

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tower::buffer::Buffer;
use tower::util::BoxService;
use tower::{Service, ServiceExt};
use zaino_primitives::types::{Announced, PeerTxId};
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::BlockHash;
use zebra_chain::block::{Block, CountedHeader};
use zebra_chain::chain_tip::NoChainTip;
use zebra_chain::parameters::Network;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_chain::transaction::UnminedTx;
use zebra_network::types::PeerServices;
use zebra_network::{BoxError, InventoryResponse, Request, Response};

use zaino_source::testing::{raw_transaction, within};

use crate::wire;
use crate::{PeerConfig, PeerError, PeerNetwork, PushError};

type FakePeers = Buffer<BoxService<Request, Response, BoxError>, Request>;
type FakeBook = Arc<Mutex<zebra_network::AddressBook>>;

/// Full zebra-network regtest node on a fresh loopback port, answering peers with `inbound`
/// (no seeds: accepts only)
///
/// - its peer set reaches whoever connected
/// - full node, not an isolated connection (those advertise no services: a peer set refuses
///   an outbound peer serving nothing)
async fn fake_node<S>(inbound: S) -> (SocketAddr, FakePeers, FakeBook)
where
    S: Service<Request, Response = Response, Error = BoxError> + Clone + Send + Sync + 'static,
    S::Future: Send + 'static,
{
    let listen = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let listen_addr = listen.local_addr().expect("bound");
    drop(listen);
    let config = zebra_network::Config {
        network: Network::new_regtest(Default::default()),
        listen_addr,
        initial_testnet_peers: Default::default(),
        cache_dir: zebra_network::CacheDir::disabled(),
        ..zebra_network::Config::default()
    };
    let (peers, book, _) = zebra_network::init(
        config,
        inbound,
        NoChainTip,
        "/FakePeer:0/".to_owned(),
        PeerServices::NODE_NETWORK,
    )
    .await;
    (listen_addr, peers, book)
}

/// Zaino's peer set seeded with `fake` alone, back once `fake` is live (handshaken)
async fn zaino_peered_with(fake: SocketAddr) -> PeerNetwork {
    let mut config = PeerConfig::new(NetworkType::Regtest);
    config.initial_peers = vec![fake.to_string()];
    config.request_timeout = Duration::from_secs(10);
    let peers = PeerNetwork::start(config).await;
    within("the fake node handshaken (live)", async {
        while !peers.live().contains(&fake) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    peers
}

fn mainnet_blocks_1_to_3() -> Vec<Arc<Block>> {
    [
        zebra_test::vectors::BLOCK_MAINNET_1_BYTES.as_slice(),
        zebra_test::vectors::BLOCK_MAINNET_2_BYTES.as_slice(),
        zebra_test::vectors::BLOCK_MAINNET_3_BYTES.as_slice(),
    ]
    .map(|bytes| Arc::new(Block::zcash_deserialize(bytes).expect("mainnet vector")))
    .to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn zaino_is_admitted_advertising_no_services_and_reads_headers_and_blocks_byte_identical() {
    let blocks = mainnet_blocks_1_to_3();
    let served = blocks.clone();
    let inbound = tower::service_fn(move |request: Request| {
        let response = match request {
            Request::FindHeaders { .. } => Response::BlockHeaders(
                served
                    .iter()
                    .map(|block| CountedHeader { header: Arc::clone(&block.header) })
                    .collect(),
            ),
            Request::BlocksByHash(hashes) => Response::Blocks(
                hashes
                    .into_iter()
                    .map(|hash| match served.iter().find(|block| block.hash() == hash) {
                        Some(block) => InventoryResponse::Available((Arc::clone(block), None)),
                        None => InventoryResponse::Missing(hash),
                    })
                    .collect(),
            ),
            // zebra's `init` awaits one crawl of its seeds: a real peer answers `getaddr`
            Request::Peers => Response::Peers(Vec::new()),
            _ => Response::Nil,
        };
        std::future::ready(Ok::<_, BoxError>(response))
    });
    let (fake, _, fake_book) = fake_node(inbound).await;
    let peers = zaino_peered_with(fake).await;

    let seen: Vec<_> = {
        let book = fake_book.lock().expect("fake address book");
        book.peers().filter(|meta| meta.is_inbound()).map(|meta| meta.services()).collect()
    };
    assert_eq!(seen, [Some(PeerServices::empty())], "the fake's record of Zaino: no services");

    let hash = |block: &Arc<Block>| wire::block_hash(block.hash());
    let headers = peers.find_headers(&[hash(&blocks[0])]).await.expect("headers answered");
    assert_eq!(headers.len(), 3, "headers after block 1's hash: all three the fake serves");
    for (height, (header, block)) in (1..).zip(headers.iter().zip(&blocks)) {
        let expected = block.header.zcash_serialize_to_vec().expect("vec");
        assert!(*header == expected, "header {height}: byte-identical to the mainnet vector");
    }

    let fetched = peers.block_by_hash(hash(&blocks[1])).await.expect("answered");
    let fetched = fetched.expect("block 2 supplied");
    let vector = zebra_test::vectors::BLOCK_MAINNET_2_BYTES.as_slice();
    assert_eq!(fetched.peer, Some(fake), "block 2's sender");
    let (got, want) = (fetched.bytes.len(), vector.len());
    assert!(fetched.bytes == *vector, "block 2 byte-identical: {got} bytes vs the vector's {want}");
    let unknown = peers.block_by_hash(BlockHash([7; 32])).await;
    assert!(matches!(unknown, Ok(None)), "a hash no peer holds = not supplied: {unknown:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peers_mempool_is_its_own_listing_and_a_peer_not_in_the_set_is_a_failure() {
    let txs: Vec<UnminedTx> = [1, 2, 3]
        .map(|lock_time| wire::unmined_tx(&raw_transaction(lock_time, 0).1).expect("real tx"))
        .to_vec();
    let listed_ids: Vec<_> = txs.iter().map(|tx| tx.id).collect();
    let inbound = tower::service_fn(move |request: Request| {
        let response = match request {
            Request::MempoolTransactionIds => Response::TransactionIds(listed_ids.clone()),
            Request::Peers => Response::Peers(Vec::new()),
            _ => Response::Nil,
        };
        std::future::ready(Ok::<_, BoxError>(response))
    });
    let (fake, _, _) = fake_node(inbound).await;
    let peers = zaino_peered_with(fake).await;

    let listed: HashSet<PeerTxId> =
        peers.mempool_of(fake).await.expect("its mempool").into_iter().collect();
    let ids: HashSet<PeerTxId> = txs.iter().map(|tx| wire::peer_tx_id(tx.id)).collect();
    assert_eq!(listed, ids, "{fake}'s listing vs its three mempool ids");
    let stranger = SocketAddr::from(([127, 0, 0, 9], 1));
    let refused = peers.mempool_of(stranger).await;
    assert!(
        matches!(refused, Err(PeerError::Network(_))),
        "{stranger} not in the set: a failure, never another peer's listing: {refused:?}"
    );
}

/// Fake: B's bytes for an ask of A
#[tokio::test(flavor = "multi_thread")]
async fn a_lying_peers_answer_never_labels_the_asked_id_and_is_an_answer_not_a_failure() {
    let raws = [1, 2, 3].map(|lock_time| raw_transaction(lock_time, 0).1);
    let txs: Vec<UnminedTx> =
        raws.iter().map(|raw| wire::unmined_tx(raw).expect("real tx")).collect();
    let (a, b, c) = (txs[0].clone(), txs[1].clone(), txs[2].clone());
    let served = (a.id, b, c.clone());
    let inbound = tower::service_fn(move |request: Request| {
        let (lie_for, lie, honest) = served.clone();
        let response = match request {
            Request::TransactionsById(ids) => Response::Transactions(
                ids.into_iter()
                    .map(|id| match id {
                        _ if id == lie_for => InventoryResponse::Available((lie.clone(), None)),
                        _ if id == honest.id => {
                            InventoryResponse::Available((honest.clone(), None))
                        }
                        _ => InventoryResponse::Missing(id),
                    })
                    .collect(),
            ),
            Request::Peers => Response::Peers(Vec::new()),
            _ => Response::Nil,
        };
        std::future::ready(Ok::<_, BoxError>(response))
    });
    let (fake, _, _) = fake_node(inbound).await;
    let peers = zaino_peered_with(fake).await;

    let (id_a, id_c) = (wire::peer_tx_id(a.id), wire::peer_tx_id(c.id));
    let lied = peers.transactions_by_id(&[id_a]).await.expect("an answer, not a failure");
    assert_eq!(lied, [(id_a, None)], "A asked, B's bytes sent: A unsupplied");
    let honest = peers.transactions_by_id(&[id_c]).await.expect("an answer");
    assert_eq!(honest, [(id_c, Some(raws[2].clone()))], "C asked, C's bytes sent");
    // zebra ends the reply at the unsolicited B: C rides it only if it came first
    let both = peers.transactions_by_id(&[id_a, id_c]).await.expect("an answer, not a failure");
    assert_eq!(both[0], (id_a, None), "A and C asked together: A unsupplied");
    let c_honest = both[1].1.as_ref().is_none_or(|raw| *raw == raws[2]);
    assert!(c_honest, "A and C asked together: C = its own bytes or unsupplied: {both:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_block_inv_arrives_attributed_to_its_sender_and_a_transaction_inv_is_ignored() {
    let block_2 = Arc::clone(&mainnet_blocks_1_to_3()[1]);
    let inbound = tower::service_fn(|request: Request| {
        let response = match request {
            Request::Peers => Response::Peers(Vec::new()),
            _ => Response::Nil,
        };
        std::future::ready(Ok::<_, BoxError>(response))
    });
    let (fake, mut fake_peers, _) = fake_node(inbound).await;
    let peers = zaino_peered_with(fake).await;
    let mut announcements = peers.announcements();

    let tx = wire::unmined_tx(&raw_transaction(1, 0).1).expect("real tx");
    let advertise = Request::AdvertiseTransactionIds([tx.id].into_iter().collect(), None);
    fake_peers.ready().await.expect("ready").call(advertise).await.expect("tx inv sent");
    let advertise = Request::AdvertiseBlock(block_2.hash(), None);
    fake_peers.ready().await.expect("ready").call(advertise).await.expect("block inv sent");

    let heard = within("the first announcement", announcements.recv()).await;
    let block = Announced { peer: fake, hash: wire::block_hash(block_2.hash()) };
    assert_eq!(heard, Ok(block), "first announced = block 2 from {fake} (tx inv skipped)");
}

/// - `call` records on the test body's thread (thread-local recorder, every request awaited here)
/// - zebra's local refusals (unknown hash → notfound, stranger → no ready peer) never hit the wire
#[tokio::test(flavor = "multi_thread")]
async fn only_answered_requests_are_timed_and_zebras_local_refusals_count_as_failures() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let rendered = recorder.handle();
    let _recording = metrics::set_default_local_recorder(&recorder);
    let blocks = mainnet_blocks_1_to_3();
    let listed = wire::unmined_tx(&raw_transaction(1, 0).1).expect("real tx").id;
    let served = blocks.clone();
    let inbound = tower::service_fn(move |request: Request| {
        let response = match request {
            Request::FindHeaders { .. } => Response::BlockHeaders(
                served
                    .iter()
                    .map(|block| CountedHeader { header: Arc::clone(&block.header) })
                    .collect(),
            ),
            Request::BlocksByHash(hashes) => Response::Blocks(
                hashes
                    .into_iter()
                    .map(|hash| match served.iter().find(|block| block.hash() == hash) {
                        Some(block) => InventoryResponse::Available((Arc::clone(block), None)),
                        None => InventoryResponse::Missing(hash),
                    })
                    .collect(),
            ),
            Request::MempoolTransactionIds => Response::TransactionIds(vec![listed]),
            Request::Peers => Response::Peers(Vec::new()),
            _ => Response::Nil,
        };
        std::future::ready(Ok::<_, BoxError>(response))
    });
    let (fake, _, _) = fake_node(inbound).await;
    let peers = zaino_peered_with(fake).await;

    let block_1 = wire::block_hash(blocks[0].hash());
    peers.find_headers(&[block_1]).await.expect("answered");
    peers.block_by_hash(block_1).await.expect("answered");
    peers.block_by_hash(BlockHash([7; 32])).await.expect("answered: not supplied");
    peers.mempool_of(fake).await.expect("answered");
    peers.mempool_of(SocketAddr::from(([127, 0, 0, 9], 1))).await.expect_err("not in the set");

    let scrape = rendered.render();
    let missing: Vec<&str> = [
        r#"zaino_peers_request_duration_seconds_count{request="FindHeaders"} 1"#,
        r#"zaino_peers_request_duration_seconds_count{request="BlocksByHash"} 1"#,
        r#"zaino_peers_request_duration_seconds_count{request="MempoolTransactionIds"} 1"#,
        r#"zaino_peers_request_failures_total{request="BlocksByHash",failure="peer"} 1"#,
        r#"zaino_peers_request_failures_total{request="MempoolTransactionIds",failure="peer"} 1"#,
    ]
    .into_iter()
    .filter(|line| !scrape.lines().any(|scraped| scraped == *line))
    .collect();
    assert!(missing.is_empty(), "missing {missing:?} from:\n{scrape}");
}

/// - isolated second connection from our IP = dropped by zebra's one-per-IP limit: the push must
///   ride the peer set's existing one
/// - malformed bytes first, then the good ones: the fake's record = the good ones alone
#[tokio::test(flavor = "multi_thread")]
async fn a_push_reaches_a_connected_peer_byte_identical_and_malformed_bytes_never_leave() {
    let pushed: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let recorded = Arc::clone(&pushed);
    let inbound = tower::service_fn(move |request: Request| {
        let response = match request {
            Request::PushTransaction(tx, _) => {
                recorded.lock().expect("pushed").push(wire::transaction_bytes(&tx));
                Response::Nil
            }
            Request::Peers => Response::Peers(Vec::new()),
            _ => Response::Nil,
        };
        std::future::ready(Ok::<_, BoxError>(response))
    });
    let (fake, _, _) = fake_node(inbound).await;
    let peers = zaino_peered_with(fake).await;
    let (expected_txid, raw) = raw_transaction(42, 0);

    let malformed = peers.push(&raw[..raw.len() - 1]).await;
    let txid = peers.push(&raw).await.expect("pushed");

    assert!(matches!(malformed, Err(PushError::Wire(_))), "truncated: refused: {malformed:?}");
    assert_eq!(txid, expected_txid, "push's txid vs zcash_primitives' of the same bytes");
    within("the push lands", async {
        while pushed.lock().expect("pushed").is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let landed = pushed.lock().expect("pushed").clone();
    assert!(landed == [raw.clone()], "{} pushes landed, want the good bytes once", landed.len());
}

/// - `call` records on the test body's thread (thread-local recorder, the push awaited here)
/// - a push never timed: zebra's fan-out answers `Nil` itself, no peer replies
#[tokio::test(flavor = "multi_thread")]
async fn a_push_with_no_ready_peer_is_a_peers_timeout_counted_never_timed() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let rendered = recorder.handle();
    let _recording = metrics::set_default_local_recorder(&recorder);
    let mut alone = PeerConfig::new(NetworkType::Regtest);
    alone.request_timeout = Duration::from_secs(1);
    let peers = PeerNetwork::start(alone).await;

    let unpeered = peers.push(&raw_transaction(42, 0).1).await;

    assert!(
        matches!(unpeered, Err(PushError::Peers(PeerError::Timeout(_)))),
        "no peer: a Peers timeout, never a verdict: {unpeered:?}"
    );
    let scrape = rendered.render();
    let timeout =
        r#"zaino_peers_request_failures_total{request="PushTransaction",failure="timeout"} 1"#;
    assert!(scrape.lines().any(|line| line == timeout), "{timeout} missing from:\n{scrape}");
    assert!(!scrape.contains("zaino_peers_request_duration_seconds"), "a push timed:\n{scrape}");
}
