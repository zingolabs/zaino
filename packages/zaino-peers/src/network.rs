//! zebra-network embedded as Zaino's p2p layer (`docs/architecture.md` §3)
//!
//! ```text
//!   requests ──▶ peer set (zebra's p2c over ready peers) ──▶ FindHeaders · BlocksByHash
//!                                                            TransactionsById
//!   mempool_of(peer) ──▶ that peer alone ──▶ its whole mempool listing
//!   push(raw) ──▶ PushTransaction ──▶ ⌈ready/3⌉ random ready peers (zebra's relay)
//!   peers ──▶ inbound service ──▶ AdvertiseBlock(hash, peer) ──▶ announcements (broadcast)
//!                                  everything else ──▶ Nil (Zaino serves no peer)
//! ```
//!
//! - every id or hash handed out = zebra's own from the bytes; a peer answering with something
//!   not asked for is dropped and scored as misbehaviour (zebra's address book bans on it)

use std::collections::HashMap;
use std::hash::Hash;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tower::buffer::Buffer;
use tower::util::BoxService;
use tower::{Service, ServiceExt};
use tracing::debug;
use zaino_primitives::types::{Announced, PeerTxId};
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::{BlockHash, TxId};
use zebra_chain::chain_tip::NoChainTip;
use zebra_chain::parameters::Network;
use zebra_network::types::PeerServices;
use zebra_network::{
    AddressBook, BoxError, CacheDir, Config, InventoryResponse, MisbehaviorReport, PeerSocketAddr,
    Request, Response, SharedPeerError,
};

use crate::wire::{self, WireError};

/// Score for one answer that was not what was asked (zebra bans past its threshold)
const LIED: u32 = 50;

/// Announcements buffered per subscriber before the slowest one lags (and learns it did)
const ANNOUNCED_BUFFER: usize = 4_096;

/// Outbound peers kept by default (each = one inbound slot on a public node: stay modest)
pub const DEFAULT_PEER_TARGET: NonZeroUsize = NonZeroUsize::new(8).expect("8 is non-zero");

/// - `listen_addr` = zebra's listener (Zaino serves no peer: keep it on loopback)
/// - `initial_peers` `None` = the network's DNS seeders (regtest: loopback addresses only)
/// - `cache_dir` = zebra's peer address cache (`None` = every start re-seeds)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConfig {
    pub network: NetworkType,
    pub listen_addr: SocketAddr,
    pub initial_peers: Option<Vec<String>>,
    pub peer_target: NonZeroUsize,
    pub request_timeout: Duration,
    pub cache_dir: Option<PathBuf>,
}

impl PeerConfig {
    pub fn new(network: NetworkType) -> Self {
        Self {
            network,
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            initial_peers: None,
            peer_target: DEFAULT_PEER_TARGET,
            request_timeout: Duration::from_secs(20),
            cache_dir: None,
        }
    }
}

/// One block's bytes + its sender (`None` = zebra named none): [`PeerNetwork::blame`] it on a lie
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supplied {
    pub bytes: Vec<u8>,
    pub peer: Option<SocketAddr>,
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("no peer answered within {0:?}")]
    Timeout(Duration),
    #[error("peer request failed: {0}")]
    Network(BoxError),
    #[error("peer answered {got} to {asked}")]
    Unexpected { asked: &'static str, got: String },
}

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Peers(#[from] PeerError),
}

type PeerSet = Buffer<BoxService<Request, Response, BoxError>, Request>;

/// One running zebra-network peer set
pub struct PeerNetwork {
    peers: PeerSet,
    address_book: Arc<std::sync::Mutex<AddressBook>>,
    misbehaved: mpsc::Sender<MisbehaviorReport>,
    announced: broadcast::Sender<Announced>,
    timeout: Duration,
}

impl PeerNetwork {
    /// Back once the seeds are dialed + crawled once (zebra's `init`: seconds, up to its crawl
    /// timeout for a seed never answering `getaddr`); crawling continues in the background
    ///
    /// - multi-thread runtime required (zebra's codec parses in `block_in_place`)
    pub async fn start(config: PeerConfig) -> Self {
        let network = match config.network {
            NetworkType::Main => Network::Mainnet,
            NetworkType::Test => Network::new_default_testnet(),
            NetworkType::Regtest => Network::new_regtest(Default::default()),
        };
        let mut zebra = Config {
            network,
            listen_addr: config.listen_addr,
            cache_dir: config.cache_dir.map_or(CacheDir::disabled(), CacheDir::custom_path),
            peerset_initial_target_size: config.peer_target.get(),
            ..Config::default()
        };
        if let Some(initial) = config.initial_peers {
            zebra.initial_mainnet_peers = initial.iter().cloned().collect();
            zebra.initial_testnet_peers = initial.into_iter().collect();
        }
        let (announced, _) = broadcast::channel(ANNOUNCED_BUFFER);
        let user_agent = format!("/Zaino:{}/", env!("CARGO_PKG_VERSION"));
        // no services: Zaino serves no peer (claiming NODE_NETWORK draws block requests it drops)
        let (peers, address_book, misbehaved) = zebra_network::init(
            zebra,
            inbound(announced.clone()),
            NoChainTip,
            user_agent,
            PeerServices::empty(),
        )
        .await;
        let timeout = config.request_timeout;
        Self { peers, address_book, misbehaved, announced, timeout }
    }

    /// Every block `inv` a connected peer sends from now on
    ///
    /// - transaction `inv`s ignored: a node announces each to a third of its peers, once (mempool
    ///   membership = [`PeerNetwork::mempool_of`] instead)
    pub fn announcements(&self) -> broadcast::Receiver<Announced> {
        self.announced.subscribe()
    }

    /// Outbound peers live in the last few minutes (the ones whose `inv` arrives)
    pub fn live(&self) -> Vec<SocketAddr> {
        let now = chrono::Utc::now();
        let book = self.address_book.lock().expect("zebra address book mutex poisoned");
        book.peers()
            .filter(|meta| !meta.is_inbound() && meta.was_recently_live(now))
            .map(|meta| *meta.addr())
            .collect()
    }

    /// Headers after the first of `known` a peer holds (best-chain order), up to `stop`
    pub async fn find_headers(
        &self,
        known: &[BlockHash],
        stop: Option<BlockHash>,
    ) -> Result<Vec<Vec<u8>>, PeerError> {
        let known_blocks = known.iter().copied().map(wire::zebra_block_hash).collect();
        let stop = stop.map(wire::zebra_block_hash);
        match self.call(Request::FindHeaders { known_blocks, stop }).await? {
            Response::BlockHeaders(headers) => Ok(headers.iter().map(wire::header_bytes).collect()),
            other => Err(unexpected("FindHeaders", &other)),
        }
    }

    /// Each asked block, in `hashes` order (`None` = not supplied)
    ///
    /// - block hashing to something not asked for → dropped, sender scored
    /// - body vs its header = the caller's check ([`blame`](Self::blame) the sender on a lie)
    pub async fn blocks_by_hash(
        &self,
        hashes: &[BlockHash],
    ) -> Result<Vec<(BlockHash, Option<Supplied>)>, PeerError> {
        let asked = hashes.iter().copied().map(wire::zebra_block_hash).collect();
        let items = match self.inventory(Request::BlocksByHash(asked)).await? {
            Some(Response::Blocks(items)) => items,
            Some(other) => return Err(unexpected("BlocksByHash", &other)),
            None => Vec::new(),
        };
        Ok(self.answered(hashes, items, "a block not asked for", |block, peer| {
            let supplied = Supplied { bytes: wire::block_bytes(&block), peer: peer.map(|p| *p) };
            (wire::block_hash(block.hash()), supplied)
        }))
    }

    /// `peer` served something its own bytes refute (scored like any lie: zebra bans past its
    /// threshold)
    pub fn blame(&self, peer: SocketAddr, what: &str) {
        self.lied(Some(PeerSocketAddr::from(peer)), what);
    }

    /// `peer`'s whole mempool (`mempool` → `inv`): every transaction it admitted, attributable
    ///
    /// - `peer` busy or gone = `Network` (asked again next round), never another peer's listing
    /// - an `inv` announcement landing first is taken as the reply (the protocol cannot tell
    ///   them apart): one short listing, the next poll's whole
    pub async fn mempool_of(&self, peer: SocketAddr) -> Result<Vec<PeerTxId>, PeerError> {
        match self.call(Request::MempoolTransactionIdsFrom(peer.into())).await? {
            Response::TransactionIds(ids) => Ok(ids.into_iter().map(wire::peer_tx_id).collect()),
            // zebrad's empty mempool
            Response::Nil => Ok(Vec::new()),
            other => Err(unexpected("MempoolTransactionIdsFrom", &other)),
        }
    }

    /// Each asked transaction's bytes, in `ids` order (`None` = not supplied)
    ///
    /// - bytes hashing to an id not asked for → dropped, sender scored
    /// - unsolicited tx ends zebra's request: later ones in that reply lost too (`None`, re-asked)
    pub async fn transactions_by_id(
        &self,
        ids: &[PeerTxId],
    ) -> Result<Vec<(PeerTxId, Option<Vec<u8>>)>, PeerError> {
        let asked = ids.iter().copied().map(wire::unmined_tx_id).collect();
        let items = match self.inventory(Request::TransactionsById(asked)).await? {
            Some(Response::Transactions(items)) => items,
            Some(other) => return Err(unexpected("TransactionsById", &other)),
            None => Vec::new(),
        };
        Ok(self.answered(ids, items, "a transaction not asked for", |tx, _| {
            (wire::peer_tx_id(tx.id), wire::transaction_bytes(&tx))
        }))
    }

    /// `raw` relayed as zebrad relays: `PushTransaction` over the peer set's own connections to
    /// ⌈ready/3⌉ random ready peers; txid = the bytes' own
    ///
    /// - unattributed: zebra names no recipient, and a peer answers a push with nothing
    /// - no ready peer within the timeout = `Peers(Timeout)`
    pub async fn push(&self, raw: &[u8]) -> Result<TxId, PushError> {
        let tx = wire::unmined_tx(raw)?;
        let txid = wire::peer_tx_id(tx.id).txid;
        self.call(Request::PushTransaction(tx, None)).await?;
        Ok(txid)
    }

    /// Inventory request; `None` = nothing asked for supplied
    ///
    /// - zebra ends one in `SharedPeerError` (`notfound` or an unsolicited reply first:
    ///   `peer/connection.rs`), not a `Missing` list; any other error = failed
    async fn inventory(&self, request: Request) -> Result<Option<Response>, PeerError> {
        match self.call(request).await {
            Ok(answer) => Ok(Some(answer)),
            Err(PeerError::Network(cause)) if cause.downcast_ref::<SharedPeerError>().is_some() => {
                Ok(None)
            }
            Err(failed) => Err(failed),
        }
    }

    /// Each `asked` key's value, `asked` order (`None` = not supplied)
    ///
    /// - `keyed` = key from the item's own bytes; key not asked for = dropped, sender scored
    fn answered<K, A, M, V>(
        &self,
        asked: &[K],
        items: Vec<InventoryResponse<(A, Option<PeerSocketAddr>), M>>,
        what: &str,
        keyed: impl Fn(A, Option<PeerSocketAddr>) -> (K, V),
    ) -> Vec<(K, Option<V>)>
    where
        K: Copy + Eq + Hash,
    {
        let mut found = HashMap::new();
        for item in items {
            let InventoryResponse::Available((answer, peer)) = item else { continue };
            let (key, value) = keyed(answer, peer);
            match asked.contains(&key) {
                true => {
                    found.insert(key, value);
                }
                false => self.lied(peer, what),
            }
        }
        asked.iter().map(|key| (*key, found.remove(key))).collect()
    }

    async fn call(&self, request: Request) -> Result<Response, PeerError> {
        let mut peers = self.peers.clone();
        let answered = async { peers.ready().await?.call(request).await };
        match tokio::time::timeout(self.timeout, answered).await {
            Ok(answer) => answer.map_err(PeerError::Network),
            Err(_) => Err(PeerError::Timeout(self.timeout)),
        }
    }

    fn lied(&self, peer: Option<PeerSocketAddr>, what: &str) {
        let Some(peer) = peer else { return };
        debug!(%peer, what, "Peer answered with something not asked for");
        let _ = self.misbehaved.try_send(MisbehaviorReport::new(peer, LIED, what));
    }
}

/// Answers every peer with nothing; reports each block `inv` with its sender
fn inbound(
    announced: broadcast::Sender<Announced>,
) -> impl Service<
    Request,
    Response = Response,
    Error = BoxError,
    Future = std::future::Ready<Result<Response, BoxError>>,
> + Clone
       + Send
       + Sync
       + 'static {
    tower::service_fn(move |request: Request| {
        if let Request::AdvertiseBlock(hash, Some(peer)) = request {
            let _ = announced.send(Announced { peer: *peer, hash: wire::block_hash(hash) });
        }
        std::future::ready(Ok(Response::Nil))
    })
}

fn unexpected(asked: &'static str, got: &Response) -> PeerError {
    PeerError::Unexpected { asked, got: format!("{got:?}").chars().take(64).collect() }
}
