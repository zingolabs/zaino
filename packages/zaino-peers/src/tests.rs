//! Zaino's peer set against a scripted zebra-network peer on loopback (real handshakes, real
//! codec): no DNS, no internet

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

use zaino_source::testing::raw_transaction;

use crate::wire;
use crate::{PeerConfig, PeerError, PeerNetwork, PushError, Supplied};

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

async fn until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never {what}");
}

/// Fake peer: mainnet blocks 1–3 + three mempool txs, lying when asked for tx A (answers B)
///
/// - Zaino admitted while advertising no services
/// - headers + blocks byte-identical, unknown hash → `None`
/// - its mempool asked by address = its three ids
/// - peer not in the set = a failure, never another's listing
/// - lie = an answer, not a failure (A = `None`, B never labelled A)
/// - its block `inv` attributed to its address (a transaction `inv` = nothing), peer = live
// multi_thread required: zebra-network's codec parses blocks and txs in `block_in_place`
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_serves_headers_blocks_and_mempool_its_lies_are_dropped_and_its_inv_is_attributed() {
    let blocks: Vec<Arc<Block>> = [
        zebra_test::vectors::BLOCK_MAINNET_1_BYTES.as_slice(),
        zebra_test::vectors::BLOCK_MAINNET_2_BYTES.as_slice(),
        zebra_test::vectors::BLOCK_MAINNET_3_BYTES.as_slice(),
    ]
    .map(|bytes| Arc::new(Block::zcash_deserialize(bytes).expect("mainnet vector")))
    .to_vec();
    let raws = [1, 2, 3].map(|lock_time| raw_transaction(lock_time, 0).1);
    let txs: Vec<UnminedTx> =
        raws.iter().map(|raw| wire::unmined_tx(raw).expect("real tx")).collect();
    let (a, b, c) = (txs[0].clone(), txs[1].clone(), txs[2].clone());

    let served = (blocks.clone(), txs.clone(), a.id, b.clone());
    let inbound = tower::service_fn(move |request: Request| {
        let (blocks, txs, lie_for, lie) = served.clone();
        let response = match request {
            Request::FindHeaders { .. } => Response::BlockHeaders(
                blocks
                    .iter()
                    .map(|block| CountedHeader { header: Arc::clone(&block.header) })
                    .collect(),
            ),
            Request::BlocksByHash(hashes) => Response::Blocks(
                hashes
                    .into_iter()
                    .map(|hash| match blocks.iter().find(|block| block.hash() == hash) {
                        Some(block) => InventoryResponse::Available((Arc::clone(block), None)),
                        None => InventoryResponse::Missing(hash),
                    })
                    .collect(),
            ),
            Request::MempoolTransactionIds => {
                Response::TransactionIds(txs.iter().map(|tx| tx.id).collect())
            }
            Request::TransactionsById(ids) => Response::Transactions(
                ids.into_iter()
                    .map(|id| match txs.iter().find(|tx| tx.id == id) {
                        Some(_) if id == lie_for => {
                            InventoryResponse::Available((lie.clone(), None))
                        }
                        Some(tx) => InventoryResponse::Available((tx.clone(), None)),
                        None => InventoryResponse::Missing(id),
                    })
                    .collect(),
            ),
            // zebra's `init` awaits one crawl of its seeds: a real peer answers `getaddr`
            Request::Peers => Response::Peers(Vec::new()),
            _ => Response::Nil,
        };
        std::future::ready(Ok::<_, BoxError>(response))
    });
    let (fake, mut fake_peers, fake_book) = fake_node(inbound).await;

    let mut config = PeerConfig::new(NetworkType::Regtest);
    config.initial_peers = Some(vec![fake.to_string()]);
    config.request_timeout = Duration::from_secs(10);
    let peers = PeerNetwork::start(config).await;
    let mut announcements = peers.announcements();
    until("handshaken (live)", || peers.live().contains(&fake)).await;
    let seen: Vec<_> = {
        let book = fake_book.lock().expect("fake address book");
        book.peers().filter(|meta| meta.is_inbound()).map(|meta| meta.services()).collect()
    };
    assert_eq!(seen, [Some(PeerServices::empty())], "Zaino advertises no services, still admitted");

    let hash = |block: &Arc<Block>| wire::block_hash(block.hash());
    let headers = peers.find_headers(&[hash(&blocks[0])]).await.expect("headers");
    let expected: Vec<Vec<u8>> =
        blocks.iter().map(|block| block.header.zcash_serialize_to_vec().expect("vec")).collect();
    assert_eq!(headers, expected, "every header byte-identical, in order");

    let fetched = peers.block_by_hash(hash(&blocks[1])).await.expect("block");
    let vector = zebra_test::vectors::BLOCK_MAINNET_2_BYTES.to_vec();
    let supplied = Supplied { bytes: vector, peer: Some(fake) };
    assert_eq!(fetched, Some(supplied), "sender named");
    let unknown = peers.block_by_hash(BlockHash([7; 32])).await.expect("answered");
    assert_eq!(unknown, None, "not supplied");

    let listed: HashSet<PeerTxId> =
        peers.mempool_of(fake).await.expect("its mempool").into_iter().collect();
    let ids: HashSet<PeerTxId> = txs.iter().map(|tx| wire::peer_tx_id(tx.id)).collect();
    assert_eq!(listed, ids);
    let stranger = SocketAddr::from(([127, 0, 0, 9], 1));
    assert!(peers.mempool_of(stranger).await.is_err(), "no other peer answers for it");

    let (id_a, id_c) = (wire::peer_tx_id(a.id), wire::peer_tx_id(c.id));
    let lied = peers.transactions_by_id(&[id_a]).await.expect("an answer, not a failure");
    assert_eq!(lied, [(id_a, None)], "B's bytes never labelled A");
    let honest = peers.transactions_by_id(&[id_c]).await.expect("transactions");
    assert_eq!(honest, [(id_c, Some(raws[2].clone()))]);
    // together: zebra ends the reply at the unsolicited B, so C rides it only if it came first
    let both = peers.transactions_by_id(&[id_a, id_c]).await.expect("an answer, not a failure");
    assert_eq!(both[0], (id_a, None));
    assert!(both[1].1.as_ref().is_none_or(|raw| *raw == raws[2]), "{both:?}");

    // a transaction inv first: ignored, so the block inv is the first announcement
    let advertised: HashSet<_> = [a.id, b.id].into_iter().collect();
    let advertise = Request::AdvertiseTransactionIds(advertised, None);
    fake_peers.ready().await.expect("ready").call(advertise).await.expect("inv sent");
    let advertise = Request::AdvertiseBlock(blocks[1].hash(), None);
    fake_peers.ready().await.expect("ready").call(advertise).await.expect("inv sent");
    let heard = tokio::time::timeout(Duration::from_secs(10), announcements.recv())
        .await
        .expect("a block inv within 10 s")
        .expect("announcement");
    let block = Announced { peer: fake, hash: hash(&blocks[1]) };
    assert_eq!(heard, block, "the block's hash, sender named");
}

/// - Push → lands at a peer the peer set is already connected to (an isolated second connection
///   from our IP = dropped by zebra's one-per-IP limit), exactly the bytes, named by their txid
/// - Malformed bytes never sent
/// - No ready peer = a `Peers` failure, not a verdict
/// - Every request reaching the peer set timed by command, its timeout counted (malformed = none)
// multi_thread required: zebra-network's codec parses txs in `block_in_place`
#[tokio::test(flavor = "multi_thread")]
async fn a_push_rides_the_peer_sets_own_connection_with_the_exact_bytes() {
    // thread-local: `call` records on the test body's thread (the body awaits every push)
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let rendered = recorder.handle();
    let _recording = metrics::set_default_local_recorder(&recorder);

    let pushed: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let recorded = Arc::clone(&pushed);
    let inbound = tower::service_fn(move |request: Request| {
        let response = match request {
            Request::PushTransaction(tx, _) => {
                recorded.lock().expect("pushed").push(wire::transaction_bytes(&tx));
                Response::Nil
            }
            // zebra's `init` awaits one crawl of its seeds: a real peer answers `getaddr`
            Request::Peers => Response::Peers(Vec::new()),
            _ => Response::Nil,
        };
        std::future::ready(Ok::<_, BoxError>(response))
    });
    let (entry, _, _) = fake_node(inbound).await;

    let mut config = PeerConfig::new(NetworkType::Regtest);
    config.initial_peers = Some(vec![entry.to_string()]);
    config.request_timeout = Duration::from_secs(10);
    let peers = PeerNetwork::start(config).await;
    until("handshaken (live)", || peers.live().contains(&entry)).await;

    let (expected, raw) = raw_transaction(42, 0);
    let txid = peers.push(&raw).await.expect("pushed");
    assert_eq!(txid, wire::peer_tx_id(wire::unmined_tx(&raw).expect("real tx").id).txid);
    assert_eq!(txid, expected, "zcash_primitives' txid of the same bytes");
    until("the push lands", || !pushed.lock().expect("pushed").is_empty()).await;
    assert_eq!(*pushed.lock().expect("pushed"), std::slice::from_ref(&raw));

    let malformed = peers.push(&raw[..raw.len() - 1]).await;
    assert!(matches!(malformed, Err(PushError::Wire(_))), "{malformed:?}");
    let mut alone = PeerConfig::new(NetworkType::Regtest);
    alone.initial_peers = Some(Vec::new());
    alone.request_timeout = Duration::from_secs(1);
    let unpeered = PeerNetwork::start(alone).await.push(&raw).await;
    assert!(matches!(unpeered, Err(PushError::Peers(PeerError::Timeout(_)))), "{unpeered:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pushed.lock().expect("pushed").len(), 1, "nothing else reached the peer");

    let scrape = rendered.render();
    let missing: Vec<&str> = [
        r#"zaino_peers_request_duration_seconds_count{request="PushTransaction"} 2"#,
        r#"zaino_peers_request_failures_total{request="PushTransaction",failure="timeout"} 1"#,
    ]
    .into_iter()
    .filter(|line| !scrape.lines().any(|scraped| scraped == *line))
    .collect();
    assert!(missing.is_empty(), "missing {missing:?} from:\n{scrape}");
}
