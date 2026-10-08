//! [`MockPeers`]: the Zcash p2p network as a script (announcements the test sends, pushes recorded)

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Mutex;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::broadcast;
use zaino_primitives::types::{Height, TransactionId};
use zaino_source::{FailureMode, NonDomainError};

use crate::{Heard, ValidatorP2pSource};

/// `live` announce and take pushes (answering nothing, as a peer does); `dead` refuse a connection
pub struct MockPeers {
    live: Vec<SocketAddr>,
    dead: BTreeSet<SocketAddr>,
    pushes: Mutex<Vec<SocketAddr>>,
    announce: broadcast::Sender<Heard>,
}

impl MockPeers {
    pub fn new(
        live: impl IntoIterator<Item = SocketAddr>,
        dead: impl IntoIterator<Item = SocketAddr>,
    ) -> Self {
        Self {
            live: live.into_iter().collect(),
            dead: dead.into_iter().collect(),
            pushes: Mutex::new(Vec::new()),
            announce: broadcast::channel(64).0,
        }
    }

    /// One `inv` from `peer` (a watch subscribed first: `heard()`)
    pub fn announce(&self, peer: SocketAddr, txids: Vec<TransactionId>) {
        let heard = Heard { peer, txids };
        self.announce.send(heard).expect("a peer watch subscribed before the announcement");
    }

    /// Entries pushed to, in order
    pub fn pushes(&self) -> Vec<SocketAddr> {
        self.pushes.lock().expect("MockPeers mutex poisoned").clone()
    }
}

impl ValidatorP2pSource for MockPeers {
    fn heard(&self) -> BoxStream<'static, Heard> {
        let rx = self.announce.subscribe();
        futures::stream::unfold(rx, |mut rx| async move { Some((rx.recv().await.ok()?, rx)) })
            .boxed()
    }

    fn live(&self) -> Vec<SocketAddr> {
        self.live.clone()
    }

    fn entries(&self, _: Option<Height>) -> Vec<SocketAddr> {
        self.live.clone()
    }

    fn push(&self, entry: SocketAddr, _: Bytes) -> BoxFuture<'static, Result<(), NonDomainError>> {
        self.pushes.lock().expect("MockPeers mutex poisoned").push(entry);
        let refused = self.dead.contains(&entry);
        Box::pin(std::future::ready(match refused {
            true => Err(NonDomainError::new(FailureMode::Connection, "MockPeers: dead peer")),
            false => Ok(()),
        }))
    }
}
