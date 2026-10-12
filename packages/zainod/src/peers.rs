//! Binds `[p2p]`: zaino-peers' [`PeerNetwork`] as chainview's [`ValidatorP2pSource`] (mempool
//! listings, block `inv`s, pushes) and the balancer's [`PeerTransport`] (the peer set = one
//! member: headers + block bodies)
//!
//! - started in the background (seeds dialled + crawled: seconds); boot never waits on it, and
//!   until it is up the view hears no peer, every submission entry is a trusted validator and
//!   every ask of the peer set fails (`Connection`)

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt as _;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tracing::{info, warn};
use zaino_chainview::{Heard, ValidatorP2pSource};
use zaino_peers::{Announced, PeerConfig, PeerError, PeerNetwork, PushError};
use zaino_primitives::types::{Block, BlockHash, Height, PeerTxId};
use zaino_source::{decode_block, FailureMode, NonDomainError};
use zaino_traffic::PeerTransport;

/// The port, empty until [`start`]'s future has brought the network up
pub(crate) struct ZebraPeers {
    network: watch::Receiver<Option<Arc<PeerNetwork>>>,
}

/// The port + the future that starts the network behind it (spawn it beside the view)
pub(crate) fn start(config: PeerConfig) -> (Arc<ZebraPeers>, impl Future<Output = ()>) {
    let (up, network) = watch::channel(None);
    let starting = async move {
        let started = PeerNetwork::start(config).await;
        info!(live = started.live().len(), "Peer network up");
        up.send_replace(Some(Arc::new(started)));
    };
    (Arc::new(ZebraPeers { network }), starting)
}

impl ZebraPeers {
    fn up(&self) -> Option<Arc<PeerNetwork>> {
        self.network.borrow().clone()
    }

    fn up_or_fail(&self) -> Result<Arc<PeerNetwork>, NonDomainError> {
        let up = self.up();
        up.ok_or_else(|| NonDomainError::new(FailureMode::Connection, "peer network not up yet"))
    }
}

impl PeerTransport for ZebraPeers {
    fn headers(
        &self,
        locator: Vec<BlockHash>,
        stop: Option<BlockHash>,
    ) -> BoxFuture<'static, Result<Vec<Vec<u8>>, NonDomainError>> {
        let network = self.up_or_fail();
        Box::pin(async move { network?.find_headers(&locator, stop).await.map_err(failed) })
    }

    /// Body vs its own header checked here: a mismatch blames its sender, never returned
    fn block(&self, hash: BlockHash) -> BoxFuture<'static, Result<Block, NonDomainError>> {
        let network = self.up_or_fail();
        Box::pin(async move {
            let network = network?;
            let answered = network.blocks_by_hash(&[hash]).await.map_err(failed)?;
            let supplied = answered.into_iter().find_map(|(_, supplied)| supplied);
            let supplied = supplied
                .ok_or_else(|| NonDomainError::new(FailureMode::Connection, "no peer had it"))?;
            let block = decode_block(&supplied.bytes)
                .map_err(|cause| NonDomainError::from_cause(FailureMode::Parse, cause))?;
            match block.check_body() {
                Ok(()) => Ok(block),
                Err(mismatch) => {
                    supplied.peer.iter().for_each(|peer| network.blame(*peer, "a mutated body"));
                    Err(NonDomainError::from_cause(FailureMode::Parse, mismatch))
                }
            }
        })
    }
}

fn failed(cause: PeerError) -> NonDomainError {
    let mode = match cause {
        PeerError::Timeout(_) => FailureMode::Timeout,
        PeerError::Network(_) => FailureMode::Connection,
        PeerError::Unexpected { .. } => FailureMode::Parse,
    };
    NonDomainError::from_cause(mode, cause)
}

impl ValidatorP2pSource for ZebraPeers {
    fn heard(&self) -> BoxStream<'static, Heard> {
        let mut network = self.network.clone();
        let subscribed = async move {
            let up = network.wait_for(Option::is_some).await.ok()?;
            Some(up.as_ref()?.announcements())
        };
        futures::stream::once(subscribed)
            .filter_map(std::future::ready)
            .flat_map(|announcements| {
                futures::stream::unfold(announcements, |mut announcements| async move {
                    loop {
                        match announcements.recv().await {
                            Ok(announced) => return Some((heard(announced), announcements)),
                            Err(RecvError::Lagged(missed)) => {
                                warn!(missed, "Peer announcements lagged, peer counts undercount");
                            }
                            Err(RecvError::Closed) => return None,
                        }
                    }
                })
            })
            .boxed()
    }

    fn live(&self) -> Vec<SocketAddr> {
        self.up().map(|network| network.live()).unwrap_or_default()
    }

    fn entries(&self, tip: Option<Height>) -> Vec<SocketAddr> {
        self.up().map(|network| network.entries(tip)).unwrap_or_default()
    }

    fn push(
        &self,
        entry: SocketAddr,
        raw: Bytes,
    ) -> BoxFuture<'static, Result<(), NonDomainError>> {
        let network = self.up_or_fail();
        Box::pin(async move {
            match network?.push_isolated(entry, &raw).await {
                Ok(_) => Ok(()),
                Err(failed) => Err(NonDomainError::from_cause(mode(&failed), failed)),
            }
        })
    }

    fn mempool(
        &self,
        peer: SocketAddr,
    ) -> BoxFuture<'static, Result<Vec<PeerTxId>, NonDomainError>> {
        let network = self.up_or_fail();
        Box::pin(async move { network?.mempool_of(peer).await.map_err(failed) })
    }

    fn transactions(
        &self,
        ids: Vec<PeerTxId>,
    ) -> BoxFuture<'static, Result<Vec<Option<Bytes>>, NonDomainError>> {
        let network = self.up_or_fail();
        Box::pin(async move {
            let found = network?.transactions_by_id(&ids).await.map_err(failed)?;
            Ok(found.into_iter().map(|(_, raw)| raw.map(Bytes::from)).collect())
        })
    }
}

fn heard(announced: Announced) -> Heard {
    Heard { peer: announced.peer, hash: announced.hash }
}

fn mode(failed: &PushError) -> FailureMode {
    match failed {
        PushError::Wire(_) => FailureMode::Parse,
        PushError::Connect { .. } | PushError::Push { .. } => FailureMode::Connection,
        PushError::Timeout { .. } => FailureMode::Timeout,
    }
}
