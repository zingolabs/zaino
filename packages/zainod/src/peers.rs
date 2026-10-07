//! Binds `[p2p]` to the view: zaino-peers' [`PeerNetwork`] as chainview's [`ValidatorP2pSource`]
//!
//! - started in the background (seeds dialled + crawled: seconds); boot never waits on it, and
//!   until it is up the view hears no peer and every submission entry is a trusted validator

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
use zaino_peers::{Announced, PeerConfig, PeerNetwork, PushError};
use zaino_primitives::types::Height;
use zaino_source::{FailureMode, NonDomainError};

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
        let network = self.up();
        Box::pin(async move {
            let network = network.ok_or_else(|| {
                NonDomainError::new(FailureMode::Connection, "peer network not up yet")
            })?;
            match network.push_isolated(entry, &raw).await {
                Ok(_) => Ok(()),
                Err(failed) => Err(NonDomainError::from_cause(mode(&failed), failed)),
            }
        })
    }
}

fn heard(announced: Announced) -> Heard {
    Heard { peer: announced.peer, txids: announced.ids.into_iter().map(|id| id.txid).collect() }
}

fn mode(failed: &PushError) -> FailureMode {
    match failed {
        PushError::Wire(_) => FailureMode::Parse,
        PushError::Connect { .. } | PushError::Push { .. } => FailureMode::Connection,
        PushError::Timeout { .. } => FailureMode::Timeout,
    }
}
