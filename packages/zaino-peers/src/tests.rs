//! Zaino's peer set against a scripted zebra-network peer on loopback (real handshakes, real
//! codec): no DNS, no internet

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tower::buffer::Buffer;
use tower::util::BoxService;
use tower::{Service, ServiceExt};
use zaino_primitives::types::BlockHash;
use zcash_protocol::consensus::{BranchId, NetworkType};
use zebra_chain::block::{Block, CountedHeader};
use zebra_chain::chain_tip::NoChainTip;
use zebra_chain::parameters::Network;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_chain::transaction::UnminedTx;
use zebra_network::{BoxError, InventoryResponse, Request, Response};

use crate::wire;
use crate::{Announced, PeerConfig, PeerNetwork, PeerTxId, PushError};

/// Real empty v4 transaction, distinct per `lock_time`
fn transaction(lock_time: u32) -> Vec<u8> {
    use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
    let tx = TransactionData::<Authorized>::from_parts(
        TxVersion::V4,
        BranchId::Canopy,
        lock_time,
        0.into(),
        None,
        None,
        None,
        None,
    )
    .freeze()
    .expect("v4 freezes");
    let mut raw = Vec::new();
    tx.write(&mut raw).expect("writes");
    raw
}

type FakePeers = Buffer<BoxService<Request, Response, BoxError>, Request>;

/// Full zebra-network regtest node on a fresh loopback port, answering peers with `inbound`
/// (no seeds: accepts only); its peer set reaches whoever connected
///
/// - full node, not an isolated connection (those advertise no services: a peer set refuses
///   an outbound peer serving nothing)
async fn fake_node<S>(inbound: S) -> (SocketAddr, FakePeers)
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
    let (peers, _, _) =
        zebra_network::init(config, inbound, NoChainTip, "/FakePeer:0/".to_owned()).await;
    (listen_addr, peers)
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
/// - headers + blocks byte-identical, unknown hash → `None`
/// - mempool listing = its three ids; lie = an answer, not a failure (A = `None`, B never
///   labelled A); honest answer passes
/// - its `inv` attributed to its address; peer = submission entry candidate
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
    let raws = [transaction(1), transaction(2), transaction(3)];
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
    let (fake, mut fake_peers) = fake_node(inbound).await;

    let mut config = PeerConfig::new(NetworkType::Regtest);
    config.initial_peers = Some(vec![fake.to_string()]);
    config.request_timeout = Duration::from_secs(10);
    let peers = PeerNetwork::start(config).await;
    let mut announcements = peers.announcements();
    until("handshaken (an entry candidate)", || peers.entries(None).contains(&fake)).await;

    let hash = |block: &Arc<Block>| wire::block_hash(block.hash());
    let headers = peers.find_headers(&[hash(&blocks[0])], None).await.expect("headers");
    let expected: Vec<Vec<u8>> =
        blocks.iter().map(|block| block.header.zcash_serialize_to_vec().expect("vec")).collect();
    assert_eq!(headers, expected, "every header byte-identical, in order");

    let unknown = BlockHash::from([7; 32]);
    let fetched = peers.blocks_by_hash(&[hash(&blocks[1]), unknown]).await.expect("blocks");
    let vector = zebra_test::vectors::BLOCK_MAINNET_2_BYTES.to_vec();
    assert_eq!(fetched, [(hash(&blocks[1]), Some(vector)), (unknown, None)]);

    let listed: HashSet<PeerTxId> =
        peers.mempool_ids().await.expect("mempool").into_iter().collect();
    let ids: HashSet<PeerTxId> = txs.iter().map(|tx| wire::peer_tx_id(tx.id)).collect();
    assert_eq!(listed, ids);

    let (id_a, id_c) = (wire::peer_tx_id(a.id), wire::peer_tx_id(c.id));
    let lied = peers.transactions_by_id(&[id_a]).await.expect("an answer, not a failure");
    assert_eq!(lied, [(id_a, None)], "B's bytes never labelled A");
    let honest = peers.transactions_by_id(&[id_c]).await.expect("transactions");
    assert_eq!(honest, [(id_c, Some(raws[2].clone()))]);
    // together: zebra ends the reply at the unsolicited B, so C rides it only if it came first
    let both = peers.transactions_by_id(&[id_a, id_c]).await.expect("an answer, not a failure");
    assert_eq!(both[0], (id_a, None));
    assert!(both[1].1.as_ref().is_none_or(|raw| *raw == raws[2]), "{both:?}");

    let advertised: HashSet<_> = [a.id, b.id].into_iter().collect();
    let advertise = Request::AdvertiseTransactionIds(advertised, None);
    fake_peers.ready().await.expect("ready").call(advertise).await.expect("inv sent");
    let heard = tokio::time::timeout(Duration::from_secs(10), announcements.recv())
        .await
        .expect("an inv within 10 s")
        .expect("announcement");
    let mut heard_ids = heard.ids.clone();
    heard_ids.sort();
    let mut sent_ids = vec![wire::peer_tx_id(a.id), wire::peer_tx_id(b.id)];
    sent_ids.sort();
    assert_eq!(
        Announced { peer: heard.peer, ids: heard_ids },
        Announced { peer: fake, ids: sent_ids }
    );
}

/// - Push → exactly the chosen address, exactly the bytes, named by the bytes' own txid
/// - Malformed bytes never connect; closed address = connect failure, not a verdict
// multi_thread required: zebra-network's codec parses txs in `block_in_place`
#[tokio::test(flavor = "multi_thread")]
async fn an_isolated_push_delivers_the_exact_bytes_to_the_chosen_address() {
    let pushed: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let recorded = Arc::clone(&pushed);
    let inbound = tower::service_fn(move |request: Request| {
        if let Request::PushTransaction(tx, _) = request {
            recorded.lock().expect("pushed").push(wire::transaction_bytes(&tx));
        }
        std::future::ready(Ok::<_, BoxError>(Response::Nil))
    });
    let (entry, _) = fake_node(inbound).await;

    let mut config = PeerConfig::new(NetworkType::Regtest);
    config.initial_peers = Some(Vec::new());
    config.request_timeout = Duration::from_secs(10);
    let peers = PeerNetwork::start(config).await;

    let raw = transaction(42);
    let txid = peers.push_isolated(entry, &raw).await.expect("pushed");
    assert_eq!(txid, wire::peer_tx_id(wire::unmined_tx(&raw).expect("real tx").id).txid);
    until("the push lands", || !pushed.lock().expect("pushed").is_empty()).await;
    assert_eq!(*pushed.lock().expect("pushed"), std::slice::from_ref(&raw));

    let malformed = peers.push_isolated(entry, &raw[..raw.len() - 1]).await;
    assert!(matches!(malformed, Err(PushError::Wire(_))), "{malformed:?}");
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let closed_addr = closed.local_addr().expect("bound");
    drop(closed);
    let refused = peers.push_isolated(closed_addr, &raw).await;
    assert!(
        matches!(refused, Err(PushError::Connect { addr, .. }) if addr == closed_addr),
        "{refused:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pushed.lock().expect("pushed").len(), 1, "nothing else reached the entry");
}
