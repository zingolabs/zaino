//! zebrad's indexer push streams (`ChainTipChange` + `MempoolChange`) as wake hints
//! (`docs/architecture.md` §9)
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

use crate::indexer_endpoint::{zebra_indexer_endpoint, ZebraIndexerTls};
use crate::{EndpointError, Timeouts};

const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// One validator's indexer endpoint (`indexer_listen_addr`)
pub struct ZebraIndexerWatch {
    address: String,
    endpoint: Endpoint,
}

impl ZebraIndexerWatch {
    /// Unprobed (a validator without its indexer up is the run loop's retry)
    pub fn new(
        address: &str,
        tls: Option<&ZebraIndexerTls>,
        timeouts: Timeouts,
    ) -> Result<Self, EndpointError> {
        let endpoint = zebra_indexer_endpoint(address, tls, timeouts)?;
        Ok(Self { address: address.to_owned(), endpoint })
    }

    /// Until `cancel`: each event → `on_change`
    ///
    /// - `on_link` edges only: `true` once both streams open, `false` when either ends after
    ///   (refused connect = no edge)
    pub async fn run(
        self,
        cancel: CancellationToken,
        on_change: impl Fn(),
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
                    let zebra_indexer = &self.address;
                    warn!(%zebra_indexer, %why, "Push stream ended, polling meanwhile");
                    backoff = INITIAL_BACKOFF;
                }
                Session::Refused(why) if !announced => {
                    info!(zebra_indexer = %self.address, %why, "Push stream unavailable, polling");
                    announced = true;
                }
                Session::Refused(why) => {
                    debug!(zebra_indexer = %self.address, %why, "Push stream retry")
                }
            }
            if cancel.run_until_cancelled(tokio::time::sleep(backoff)).await.is_none() {
                return;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// One connection: both subscriptions, then events until either ends
    async fn session(&self, on_change: &impl Fn(), on_link: &impl Fn(bool)) -> Session {
        let channel: Channel = match self.endpoint.connect().await {
            Ok(channel) => channel,
            Err(e) => return Session::Refused(e.to_string()),
        };
        let mut client = IndexerClient::new(channel);
        let tips = match client.chain_tip_change(Empty {}).await {
            Ok(response) => response.into_inner().map(|item| item.map(drop)),
            Err(status) => return Session::Refused(status.to_string()),
        };
        let mempool = match client.mempool_change(Empty {}).await {
            Ok(response) => response.into_inner().map(|item| item.map(drop)),
            Err(status) => return Session::Refused(status.to_string()),
        };
        on_link(true);
        info!(zebra_indexer = %self.address, "Push stream up");

        let mut events = stream::select(tips, mempool);
        loop {
            match events.next().await {
                Some(Ok(())) => on_change(),
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
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use futures::Stream;
    use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa};
    use rcgen::{Issuer, KeyPair};
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;
    use tonic::transport::server::TcpIncoming;
    use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
    use tonic::{Request, Response, Status};
    use zaino_proto::proto::zebra_indexer::indexer_server::{Indexer, IndexerServer};
    use zaino_proto::proto::zebra_indexer::{
        BlockAndHash, BlockHashAndHeight, BlockRangeRequest, BlockRequest, MempoolChangeMessage,
        NonFinalizedStateChangeRequest,
    };

    use super::*;

    type Events<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;
    type Feed<T> = mpsc::Sender<Result<T, Status>>;

    /// Bound on each awaited event (real sockets, real clock: a failure, never a pass, waits it)
    const EVENT_WITHIN: Duration = Duration::from_secs(10);

    /// zebrad's indexer: each subscription drains a channel the test feeds (one session at a time)
    struct FakeIndexer {
        tips: Mutex<Option<mpsc::Receiver<Result<BlockHashAndHeight, Status>>>>,
        mempool: Mutex<Option<mpsc::Receiver<Result<MempoolChangeMessage, Status>>>>,
    }

    impl FakeIndexer {
        /// Next session's two streams, fed by the returned senders
        fn next_session(&self) -> (Feed<BlockHashAndHeight>, Feed<MempoolChangeMessage>) {
            let (tips, tips_rx) = mpsc::channel(8);
            let (mempool, mempool_rx) = mpsc::channel(8);
            *self.tips.lock().expect("fake") = Some(tips_rx);
            *self.mempool.lock().expect("fake") = Some(mempool_rx);
            (tips, mempool)
        }
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
        type GetBlockRangeStream = Events<BlockAndHash>;

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

        async fn get_block_range(
            &self,
            _: Request<BlockRangeRequest>,
        ) -> Result<Response<Self::GetBlockRangeStream>, Status> {
            Err(Status::unimplemented("not watched"))
        }
    }

    /// `FakeIndexer` served on a loopback port (`tls` = mutual TLS) → (address, indexer)
    async fn serve_fake_indexer(tls: Option<ServerTlsConfig>) -> (String, Arc<FakeIndexer>) {
        let indexer = Arc::new(FakeIndexer { tips: Mutex::new(None), mempool: Mutex::new(None) });
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = socket.local_addr().expect("bound").to_string();
        let mut server = Server::builder();
        if let Some(tls) = tls {
            server = server.tls_config(tls).expect("server tls");
        }
        let server = server
            .add_service(IndexerServer::from_arc(Arc::clone(&indexer)))
            .serve_with_incoming(TcpIncoming::from(socket));
        tokio::spawn(server);
        (address, indexer)
    }

    /// Loopback front: each connection reported, then piped to `backend` (`None` = dropped)
    async fn counting_front(backend: Option<String>) -> (String, mpsc::UnboundedReceiver<()>) {
        let front = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = front.local_addr().expect("bound").to_string();
        let (connected, connections) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = front.accept().await {
                let _ = connected.send(());
                let Some(backend) = backend.clone() else { continue };
                tokio::spawn(async move {
                    let mut outbound = tokio::net::TcpStream::connect(backend).await?;
                    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await
                });
            }
        });
        (address, connections)
    }

    /// `ZebraIndexerWatch` on `address`, every callback → one line on the returned channel
    fn watching(
        address: &str,
        tls: Option<&ZebraIndexerTls>,
    ) -> (mpsc::UnboundedReceiver<String>, CancellationToken, JoinHandle<()>) {
        let (seen, events) = mpsc::unbounded_channel::<String>();
        let links = seen.clone();
        let cancel = CancellationToken::new();
        let watch = ZebraIndexerWatch::new(address, tls, Timeouts::default()).expect("endpoint");
        let watching = tokio::spawn(watch.run(
            cancel.clone(),
            move || seen.send("change".to_owned()).expect("test listening"),
            move |up| links.send(format!("link {up}")).expect("test listening"),
        ));
        (events, cancel, watching)
    }

    async fn next(events: &mut mpsc::UnboundedReceiver<String>, expected: &str) -> String {
        let next = tokio::time::timeout(EVENT_WITHIN, events.recv()).await;
        let next = next.unwrap_or_else(|_| panic!("no event within {EVENT_WITHIN:?}, {expected}"));
        next.unwrap_or_else(|| panic!("watch ended before {expected}"))
    }

    async fn nth_connection(connections: &mut mpsc::UnboundedReceiver<()>, nth: usize) {
        let connected = tokio::time::timeout(EVENT_WITHIN, connections.recv()).await;
        let reason = format!("connection {nth} within {EVENT_WITHIN:?}");
        connected.expect(&reason).expect(&reason);
    }

    /// Two refused sessions (the first + its retry) seen at the front
    #[tokio::test]
    async fn a_refused_connect_is_retried_and_no_link_edge() {
        let (address, mut connections) = counting_front(None).await;

        let (mut events, cancel, watching) = watching(&address, None);
        nth_connection(&mut connections, 1).await;
        nth_connection(&mut connections, 2).await;
        cancel.cancel();
        watching.await.expect("only cancel ends the watch");

        assert_eq!(events.try_recv().ok(), None, "two refused sessions: no link, no event");
    }

    #[tokio::test]
    async fn once_both_streams_open_one_link_edge_and_every_event_a_hint() {
        let (address, indexer) = serve_fake_indexer(None).await;
        let (tips, mempool) = indexer.next_session();
        let (mut events, cancel, watching) = watching(&address, None);
        assert_eq!(next(&mut events, "the link").await, "link true", "both streams open");

        tips.send(Ok(BlockHashAndHeight { hash: vec![1; 32], height: 7 })).await.expect("open");
        mempool.send(Ok(MempoolChangeMessage::default())).await.expect("open");
        mempool.send(Ok(MempoolChangeMessage::default())).await.expect("open");

        for event in ["1 tip", "mempool 1", "mempool 2"] {
            assert_eq!(next(&mut events, event).await, "change", "{event}: a hint");
        }
        cancel.cancel();
        watching.await.expect("only cancel ends the watch");
        assert_eq!(events.try_recv().ok(), None, "three events, three hints");
    }

    /// zebrad ends a lagged stream (error, then close)
    #[tokio::test]
    async fn a_lagged_stream_drops_the_link_once_then_reconnects() {
        let (address, indexer) = serve_fake_indexer(None).await;
        let (_tips, mempool) = indexer.next_session();
        let (mut events, cancel, watching) = watching(&address, None);
        assert_eq!(next(&mut events, "the first link").await, "link true");
        let (fresh_tips, fresh_mempool) = indexer.next_session();

        mempool
            .send(Err(Status::unavailable("mempool_change channel has closed")))
            .await
            .expect("open");

        assert_eq!(next(&mut events, "the drop").await, "link false", "lag = link down");
        assert_eq!(next(&mut events, "the relink").await, "link true", "then reconnected");
        fresh_mempool.send(Ok(MempoolChangeMessage::default())).await.expect("resubscribed");
        assert_eq!(next(&mut events, "a fresh event").await, "change", "fresh session's event");
        assert!(!fresh_tips.is_closed(), "fresh session holds the fresh tip stream");
        cancel.cancel();
        watching.await.expect("only cancel ends the watch");
    }

    /// CA + server/client leaves under it, and a stranger CA's client leaf (PEM files in `dir`)
    struct Pki {
        ca: PathBuf,
        server: (PathBuf, PathBuf),
        client: (PathBuf, PathBuf),
        stranger: (PathBuf, PathBuf),
    }

    fn pki(dir: &Path) -> Pki {
        let write = |name: &str, pem: String| {
            let path = dir.join(name);
            std::fs::write(&path, pem).expect(name);
            path
        };
        let ca = |name: &str| {
            let mut params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let key = KeyPair::generate().expect("ca key");
            let cert = params.self_signed(&key).expect("ca cert");
            (write(&format!("{name}.pem"), cert.pem()), Issuer::new(params, key))
        };
        let leaf = |name: &str, usage: ExtendedKeyUsagePurpose, issuer: &Issuer<'_, KeyPair>| {
            let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
            params.extended_key_usages = vec![usage];
            let key = KeyPair::generate().expect("leaf key");
            let cert = params.signed_by(&key, issuer).expect("leaf cert");
            let cert = write(&format!("{name}.pem"), cert.pem());
            (cert, write(&format!("{name}-key.pem"), key.serialize_pem()))
        };
        let (ca_path, ours) = ca("ca");
        let (_, theirs) = ca("stranger-ca");
        Pki {
            server: leaf("server", ExtendedKeyUsagePurpose::ServerAuth, &ours),
            client: leaf("client", ExtendedKeyUsagePurpose::ClientAuth, &ours),
            stranger: leaf("stranger", ExtendedKeyUsagePurpose::ClientAuth, &theirs),
            ca: ca_path,
        }
    }

    /// zakurad's `[rpc.indexer_tls]`: server identity + client CA = `pki.ca`
    fn server_tls(pki: &Pki) -> ServerTlsConfig {
        let pem = |path: &PathBuf| std::fs::read(path).expect("pem");
        ServerTlsConfig::new()
            .identity(Identity::from_pem(pem(&pki.server.0), pem(&pki.server.1)))
            .client_ca_root(Certificate::from_pem(pem(&pki.ca)))
    }

    fn client_tls(pki: &Pki, (cert, key): &(PathBuf, PathBuf)) -> ZebraIndexerTls {
        let server_name = Some("localhost".to_owned());
        ZebraIndexerTls {
            ca_path: pki.ca.clone(),
            cert_path: cert.clone(),
            key_path: key.clone(),
            server_name,
        }
    }

    /// Mutual TLS: two handshakes (the first + its retry) seen at the front
    #[tokio::test]
    async fn a_client_certificate_from_another_ca_never_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pki = pki(dir.path());
        let (server, _indexer) = serve_fake_indexer(Some(server_tls(&pki))).await;
        let (address, mut connections) = counting_front(Some(server)).await;

        let (mut events, cancel, watching) =
            watching(&address, Some(&client_tls(&pki, &pki.stranger)));
        nth_connection(&mut connections, 1).await;
        nth_connection(&mut connections, 2).await;
        cancel.cancel();
        watching.await.expect("only cancel ends the watch");

        assert_eq!(events.try_recv().ok(), None, "stranger CA's client, 2 handshakes: no link");
    }

    #[tokio::test]
    async fn a_client_certificate_our_ca_signed_links_and_events_flow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pki = pki(dir.path());
        let (address, indexer) = serve_fake_indexer(Some(server_tls(&pki))).await;
        let (tips, _mempool) = indexer.next_session();

        let (mut events, cancel, watching) =
            watching(&address, Some(&client_tls(&pki, &pki.client)));

        assert_eq!(next(&mut events, "the link").await, "link true", "our CA's client: linked");
        tips.send(Ok(BlockHashAndHeight { hash: vec![1; 32], height: 7 })).await.expect("open");
        assert_eq!(next(&mut events, "the tip").await, "change", "events flow over TLS");
        cancel.cancel();
        watching.await.expect("only cancel ends the watch");
    }

    #[test]
    fn an_unreadable_pem_is_refused_at_construction_naming_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pki = pki(dir.path());
        let missing = ZebraIndexerTls {
            ca_path: dir.path().join("missing.pem"),
            ..client_tls(&pki, &pki.client)
        };

        let refused = ZebraIndexerWatch::new("127.0.0.1:1", Some(&missing), Timeouts::default());

        let refused = refused.map(|watch| watch.address);
        assert!(
            matches!(&refused, Err(EndpointError::TlsFile { role: "CA", path, .. }) if path.ends_with("missing.pem")),
            "missing CA file: {refused:?}"
        );
    }
}
