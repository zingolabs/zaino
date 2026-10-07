//! zebrad's indexer push streams (`ChainTipChange` + `MempoolChange`) as wake hints (`chainview.md`
//! §7)
//!
//! ```text
//!   connect ─▶ both streams open ─▶ on_link(true) ─▶ every event: on_change ─▶ either ends
//!      ▲                                                                          │
//!      └──────────── backoff 500 ms → 30 s ◀── on_link(false) ◀───────────────────┘
//! ```
//!
//! - an event carries nothing the poll relies on: a hint to read now, never data
//! - zebrad ends a stream on lag rather than drop events, so any end = reconnect + poll at once

use std::time::Duration;

use futures::stream::{self, StreamExt};
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, info, warn};
use zaino_proto::proto::zebra_indexer::indexer_client::IndexerClient;
use zaino_proto::proto::zebra_indexer::Empty;

use crate::rpc::{validator_url, EndpointError, Timeouts};

const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// What changed on the validator
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Tip,
    Mempool,
}

/// One validator's indexer endpoint (`indexer_listen_addr`; no auth on zebrad's side)
pub struct IndexerWatch {
    address: String,
    endpoint: Endpoint,
}

impl IndexerWatch {
    /// Unprobed (a validator without its indexer up is the run loop's retry)
    pub fn at(address: &str, timeouts: Timeouts) -> Result<Self, EndpointError> {
        let url = validator_url(address)?;
        let endpoint = Endpoint::from_shared(url).map_err(|e| EndpointError::Address {
            address: address.to_owned(),
            reason: e.to_string(),
        })?;
        let endpoint = endpoint.connect_timeout(timeouts.connect).tcp_nodelay(true);
        Ok(Self { address: address.to_owned(), endpoint })
    }

    /// Until `cancel`: each event → `on_change`; edges only on `on_link`: `true` once both streams
    /// are open, `false` when either ends after that (a refused connect is no edge)
    pub async fn run(
        self,
        cancel: CancellationToken,
        on_change: impl Fn(Change),
        on_link: impl Fn(bool),
    ) {
        let mut backoff = INITIAL_BACKOFF;
        let mut announced = false;
        loop {
            let ended = cancel.run_until_cancelled(self.session(&on_change, &on_link)).await;
            let Some(ended) = ended else { return };
            match ended {
                Session::Streamed(why) => {
                    on_link(false);
                    warn!(indexer = %self.address, %why, "Push stream ended, polling meanwhile");
                    backoff = INITIAL_BACKOFF;
                }
                Session::Refused(why) if !announced => {
                    info!(indexer = %self.address, %why, "Push stream unavailable, polling");
                    announced = true;
                }
                Session::Refused(why) => debug!(indexer = %self.address, %why, "Push stream retry"),
            }
            if cancel.run_until_cancelled(tokio::time::sleep(backoff)).await.is_none() {
                return;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// One connection: both subscriptions, then events until either ends
    async fn session(&self, on_change: &impl Fn(Change), on_link: &impl Fn(bool)) -> Session {
        let channel: Channel = match self.endpoint.connect().await {
            Ok(channel) => channel,
            Err(e) => return Session::Refused(e.to_string()),
        };
        let mut client = IndexerClient::new(channel);
        let tips = match client.chain_tip_change(Empty {}).await {
            Ok(response) => response.into_inner().map(|item| item.map(|_| Change::Tip)),
            Err(status) => return Session::Refused(status.to_string()),
        };
        let mempool = match client.mempool_change(Empty {}).await {
            Ok(response) => response.into_inner().map(|item| item.map(|_| Change::Mempool)),
            Err(status) => return Session::Refused(status.to_string()),
        };
        on_link(true);
        info!(indexer = %self.address, "Push stream up");

        let mut events = stream::select(tips, mempool);
        loop {
            match events.next().await {
                Some(Ok(change)) => on_change(change),
                Some(Err(status)) => return Session::Streamed(status.to_string()),
                None => return Session::Streamed("closed by the validator".to_owned()),
            }
        }
    }
}

/// How a connection ended: after streaming, or before both streams opened
enum Session {
    Streamed(String),
    Refused(String),
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use futures::Stream;
    use tokio::sync::mpsc;
    use tonic::{Request, Response, Status};
    use zaino_proto::proto::zebra_indexer::indexer_server::{Indexer, IndexerServer};
    use zaino_proto::proto::zebra_indexer::{
        BlockAndHash, BlockHashAndHeight, BlockRequest, MempoolChangeMessage,
        NonFinalizedStateChangeRequest,
    };

    use super::*;

    type Events<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

    /// zebrad's indexer: each subscription drains a channel the test feeds (one session at a time)
    struct FakeIndexer {
        tips: Mutex<Option<mpsc::Receiver<Result<BlockHashAndHeight, Status>>>>,
        mempool: Mutex<Option<mpsc::Receiver<Result<MempoolChangeMessage, Status>>>>,
    }

    fn drain<T: Send + 'static>(receiver: Option<mpsc::Receiver<Result<T, Status>>>) -> Events<T> {
        match receiver {
            Some(receiver) => {
                Box::pin(stream::unfold(receiver, |mut rx| async { Some((rx.recv().await?, rx)) }))
            }
            None => Box::pin(stream::empty()),
        }
    }

    #[tonic::async_trait]
    impl Indexer for FakeIndexer {
        type ChainTipChangeStream = Events<BlockHashAndHeight>;
        type NonFinalizedStateChangeStream = Events<BlockAndHash>;
        type MempoolChangeStream = Events<MempoolChangeMessage>;

        async fn chain_tip_change(
            &self,
            _: Request<Empty>,
        ) -> Result<Response<Self::ChainTipChangeStream>, Status> {
            Ok(Response::new(drain(self.tips.lock().expect("fake").take())))
        }

        async fn non_finalized_state_change(
            &self,
            _: Request<NonFinalizedStateChangeRequest>,
        ) -> Result<Response<Self::NonFinalizedStateChangeStream>, Status> {
            Err(Status::unimplemented("not watched"))
        }

        async fn mempool_change(
            &self,
            _: Request<Empty>,
        ) -> Result<Response<Self::MempoolChangeStream>, Status> {
            Ok(Response::new(drain(self.mempool.lock().expect("fake").take())))
        }

        async fn get_block(
            &self,
            _: Request<BlockRequest>,
        ) -> Result<Response<BlockAndHash>, Status> {
            Err(Status::unimplemented("not watched"))
        }
    }

    /// Nothing listening = refused, retried, no edge; then up: one `link true`, every event a hint;
    /// zebrad ending a lagged stream (error, then close) = one `link false`, and the watch
    /// reconnects onto fresh streams
    #[tokio::test]
    async fn events_wake_and_a_lagged_stream_drops_the_link_then_reconnects() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("bound").to_string();
        drop(listener);

        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let (changes, links) = (Arc::clone(&seen), Arc::clone(&seen));
        let cancel = CancellationToken::new();
        let watch = IndexerWatch::at(&address, Timeouts::default()).expect("address");
        let watching = tokio::spawn(watch.run(
            cancel.clone(),
            move |change| changes.lock().expect("seen").push(format!("{change:?}")),
            move |up| links.lock().expect("seen").push(format!("link {up}")),
        ));
        let events = || seen.lock().expect("seen").clone();
        let until = |want: usize| {
            let seen = Arc::clone(&seen);
            async move {
                for _ in 0..200 {
                    if seen.lock().expect("seen").len() >= want {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                panic!("never {want} events: {:?}", seen.lock().expect("seen"));
            }
        };
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(events(), Vec::<String>::new(), "refused twice: retries are no edge");

        let (tip_tx, tip_rx) = mpsc::channel(8);
        let (pool_tx, pool_rx) = mpsc::channel(8);
        let indexer = Arc::new(FakeIndexer {
            tips: Mutex::new(Some(tip_rx)),
            mempool: Mutex::new(Some(pool_rx)),
        });
        let socket = tokio::net::TcpListener::bind(&address).await.expect("rebind");
        let server = tonic::transport::Server::builder()
            .add_service(IndexerServer::from_arc(Arc::clone(&indexer)))
            .serve_with_incoming(tonic::transport::server::TcpIncoming::from(socket));
        tokio::spawn(server);

        until(1).await;
        assert_eq!(events(), ["link true"]);
        tip_tx.send(Ok(BlockHashAndHeight { hash: vec![1; 32], height: 7 })).await.expect("open");
        pool_tx.send(Ok(MempoolChangeMessage::default())).await.expect("open");
        pool_tx.send(Ok(MempoolChangeMessage::default())).await.expect("open");
        until(4).await;
        let mut hints = events().split_off(1);
        hints.sort();
        assert_eq!(hints, ["Mempool", "Mempool", "Tip"], "two streams, every event once");

        let (fresh_tip_tx, fresh_tip_rx) = mpsc::channel(8);
        let (fresh_pool_tx, fresh_pool_rx) = mpsc::channel(8);
        *indexer.tips.lock().expect("fake") = Some(fresh_tip_rx);
        *indexer.mempool.lock().expect("fake") = Some(fresh_pool_rx);
        let lagged = Status::unavailable("mempool_change channel has closed");
        pool_tx.send(Err(lagged)).await.expect("open");
        until(6).await;
        fresh_pool_tx.send(Ok(MempoolChangeMessage::default())).await.expect("resubscribed");
        until(7).await;
        assert_eq!(events()[4..], ["link false", "link true", "Mempool"], "lag = one reconnect");
        assert!(!fresh_tip_tx.is_closed(), "the new session holds the fresh tip stream");

        cancel.cancel();
        watching.await.expect("only cancel ends the watch");
    }
}
