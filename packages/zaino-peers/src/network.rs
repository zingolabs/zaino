//! zebra-network embedded as Zaino's p2p layer (`chainview.md` §8)
//!
//! ```text
//!   requests ──▶ peer set (zebra's p2c over ready peers) ──▶ FindHeaders · BlocksByHash
//!                                                            MempoolTransactionIds · TransactionsById
//!   peers ──▶ inbound service ──▶ AdvertiseTransactionIds(ids, peer) ──▶ announcements (broadcast)
//!                                  everything else ──▶ Nil (Zaino serves no peer)
//!   push_isolated(addr) ──▶ fresh connection, no node state ──▶ PushTransaction ──▶ closed
//! ```
//!
//! - every id or hash handed out = zebra's own from the bytes; a peer answering with something
//!   not asked for is dropped and scored as misbehaviour (zebra's address book bans on it)

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
use zaino_primitives::types::{BlockHash, Height, TransactionId};
use zcash_protocol::consensus::NetworkType;
use zebra_chain::block;
use zebra_chain::chain_tip::NoChainTip;
use zebra_chain::parameters::Network;
use zebra_network::{
    AddressBook, BoxError, CacheDir, Config, InventoryResponse, PeerSocketAddr, Request, Response,
    SharedPeerError, Version,
};

use crate::wire::{self, PeerTxId, WireError};

/// Score for one answer that was not what was asked (zebra bans past its threshold)
const LIED: u32 = 50;

/// Announcements buffered per subscriber before the slowest one lags (and learns it did)
const ANNOUNCED_BUFFER: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConfig {
    pub network: NetworkType,
    /// zebra's listener (Zaino serves no peer: keep it on loopback)
    pub listen_addr: SocketAddr,
    /// `None` = the network's DNS seeders; regtest takes loopback addresses only
    pub initial_peers: Option<Vec<String>>,
    pub peer_target: NonZeroUsize,
    pub request_timeout: Duration,
    /// zebra's peer address cache; `None` = no cache (every start re-seeds)
    pub cache_dir: Option<PathBuf>,
}

impl PeerConfig {
    pub fn new(network: NetworkType) -> Self {
        Self {
            network,
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            initial_peers: None,
            peer_target: NonZeroUsize::new(16).expect("16 is non-zero"),
            request_timeout: Duration::from_secs(20),
            cache_dir: None,
        }
    }
}

/// Txids one peer announced (`inv`): the `peers: x/y` sightings (`chainview.md` §5)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announced {
    pub peer: SocketAddr,
    pub ids: Vec<PeerTxId>,
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
    #[error("isolated connection to {addr}: {cause}")]
    Connect { addr: SocketAddr, cause: BoxError },
    #[error("push to {addr}: {cause}")]
    Push { addr: SocketAddr, cause: SharedPeerError },
    #[error("push to {addr} took over {timeout:?}")]
    Timeout { addr: SocketAddr, timeout: Duration },
}

type PeerSet = Buffer<BoxService<Request, Response, BoxError>, Request>;

/// One running zebra-network peer set
pub struct PeerNetwork {
    network: Network,
    peers: PeerSet,
    address_book: Arc<std::sync::Mutex<AddressBook>>,
    misbehaved: mpsc::Sender<(PeerSocketAddr, u32)>,
    announced: broadcast::Sender<Announced>,
    timeout: Duration,
    user_agent: String,
}

impl PeerNetwork {
    /// Returns once the seeds are dialed and crawled once (zebra's `init`: seconds, up to its
    /// crawl timeout for a seed that never answers `getaddr`); crawling continues in the background
    ///
    /// - needs a multi-thread runtime (zebra's codec parses in `block_in_place`)
    pub async fn start(config: PeerConfig) -> Self {
        let network = match config.network {
            NetworkType::Main => Network::Mainnet,
            NetworkType::Test => Network::new_default_testnet(),
            NetworkType::Regtest => Network::new_regtest(Default::default()),
        };
        let mut zebra = Config {
            network: network.clone(),
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
        let (peers, address_book, misbehaved) =
            zebra_network::init(zebra, inbound(announced.clone()), NoChainTip, user_agent.clone())
                .await;
        let timeout = config.request_timeout;
        Self { network, peers, address_book, misbehaved, announced, timeout, user_agent }
    }

    /// Every `inv` of transactions a connected peer sends from now on
    pub fn announcements(&self) -> broadcast::Receiver<Announced> {
        self.announced.subscribe()
    }

    /// Outbound peers live in the last few minutes: the ones whose `inv` arrives
    pub fn live(&self) -> Vec<SocketAddr> {
        self.live_where(|_| true)
    }

    /// Submission entry candidates: [`live`](Self::live) peers on a protocol version zebra
    /// accepts at `tip` (`None` = the network's initial minimum)
    pub fn entries(&self, tip: Option<Height>) -> Vec<SocketAddr> {
        let minimum = Version::min_remote_for_height(&self.network, tip.map(zebra_height));
        self.live_where(|meta| meta.negotiated_version().is_some_and(|version| version >= minimum))
    }

    fn live_where(
        &self,
        keep: impl Fn(&zebra_network::types::MetaAddr) -> bool,
    ) -> Vec<SocketAddr> {
        let now = chrono::Utc::now();
        let book = self.address_book.lock().expect("zebra address book mutex poisoned");
        book.peers()
            .filter(|meta| !meta.is_inbound() && meta.was_recently_live(now) && keep(meta))
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

    /// Each asked block's bytes, in `hashes` order (`None` = the peer did not supply it); a block
    /// that hashes to something not asked for is dropped and its sender scored
    pub async fn blocks_by_hash(
        &self,
        hashes: &[BlockHash],
    ) -> Result<Vec<(BlockHash, Option<Vec<u8>>)>, PeerError> {
        let asked = hashes.iter().copied().map(wire::zebra_block_hash).collect();
        let items = match self.inventory(Request::BlocksByHash(asked)).await? {
            Some(Response::Blocks(items)) => items,
            Some(other) => return Err(unexpected("BlocksByHash", &other)),
            None => Vec::new(),
        };
        let mut found = std::collections::HashMap::new();
        for item in items {
            let InventoryResponse::Available((block, peer)) = item else { continue };
            let hash = wire::block_hash(block.hash());
            match hashes.contains(&hash) {
                true => drop(found.insert(hash, wire::block_bytes(&block))),
                false => self.lied(peer, "a block not asked for"),
            }
        }
        Ok(hashes.iter().map(|hash| (*hash, found.remove(hash))).collect())
    }

    /// One peer's mempool listing (zebra picks the peer)
    pub async fn mempool_ids(&self) -> Result<Vec<PeerTxId>, PeerError> {
        match self.call(Request::MempoolTransactionIds).await? {
            Response::TransactionIds(ids) => Ok(ids.into_iter().map(wire::peer_tx_id).collect()),
            other => Err(unexpected("MempoolTransactionIds", &other)),
        }
    }

    /// Each asked transaction's bytes, in `ids` order (`None` = the peer did not supply it); one
    /// whose bytes hash to an id not asked for is dropped and its sender scored
    ///
    /// - an unsolicited transaction ends zebra's request: the ones after it in that reply are
    ///   lost too (`None`, asked again later)
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
        let mut found = std::collections::HashMap::new();
        for item in items {
            let InventoryResponse::Available((tx, peer)) = item else { continue };
            let id = wire::peer_tx_id(tx.id);
            match ids.contains(&id) {
                true => drop(found.insert(id, wire::transaction_bytes(&tx))),
                false => self.lied(peer, "a transaction not asked for"),
            }
        }
        Ok(ids.iter().map(|id| (*id, found.remove(id))).collect())
    }

    /// §6's per-attempt push: a fresh connection to `addr` sharing no state with this peer set,
    /// `PushTransaction`, closed; the txid = the bytes' own (a peer answers a push with nothing)
    pub async fn push_isolated(
        &self,
        addr: SocketAddr,
        raw: &[u8],
    ) -> Result<TransactionId, PushError> {
        let tx = wire::unmined_tx(raw)?;
        let txid = wire::peer_tx_id(tx.id).txid;
        let push = async {
            let connect = zebra_network::connect_isolated_tcp_direct(
                &self.network,
                addr,
                self.user_agent.clone(),
            );
            let mut client = connect.await.map_err(|cause| PushError::Connect { addr, cause })?;
            let pushed = client.ready().await.map_err(|cause| PushError::Push { addr, cause })?;
            let request = Request::PushTransaction(tx, None);
            pushed.call(request).await.map_err(|cause| PushError::Push { addr, cause })
        };
        match tokio::time::timeout(self.timeout, push).await {
            Ok(Ok(_)) => Ok(txid),
            Ok(Err(failed)) => Err(failed),
            Err(_) => Err(PushError::Timeout { addr, timeout: self.timeout }),
        }
    }

    /// An inventory request; `None` = the peer supplied nothing asked for
    ///
    /// - zebra ends such a request in `SharedPeerError` (`notfound`, or an unsolicited reply
    ///   first: `peer/connection.rs`), not a `Missing` list; any other error = it failed
    async fn inventory(&self, request: Request) -> Result<Option<Response>, PeerError> {
        match self.call(request).await {
            Ok(answer) => Ok(Some(answer)),
            Err(PeerError::Network(cause)) if cause.downcast_ref::<SharedPeerError>().is_some() => {
                Ok(None)
            }
            Err(failed) => Err(failed),
        }
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
        let _ = self.misbehaved.try_send((peer, LIED));
    }
}

/// Answers every peer with nothing; reports each transaction `inv` with its sender
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
        if let Request::AdvertiseTransactionIds(ids, Some(peer)) = request {
            let ids = ids.into_iter().map(wire::peer_tx_id).collect();
            let _ = announced.send(Announced { peer: *peer, ids });
        }
        std::future::ready(Ok(Response::Nil))
    })
}

fn zebra_height(height: Height) -> block::Height {
    block::Height(u32::from(height))
}

fn unexpected(asked: &'static str, got: &Response) -> PeerError {
    PeerError::Unexpected { asked, got: format!("{got:?}").chars().take(64).collect() }
}
