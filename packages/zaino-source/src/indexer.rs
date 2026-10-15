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

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
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

/// Mutual TLS to a validator's indexer (zakurad refuses plaintext off loopback: `[rpc.indexer_tls]`)
///
/// - `ca_path` verifies the server; `cert_path` (leaf first) + `key_path` (PKCS#8, PKCS#1 or SEC1)
///   = this client, signed by the server's client CA
/// - `server_name` = the name on the server's certificate (`None` = the address's host)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ZebraIndexerTls {
    pub ca_path: PathBuf,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub server_name: Option<String>,
}

/// `address`'s indexer endpoint, plaintext or mutual TLS; unprobed (PEMs read here: unreadable
/// = refused at boot, a rotated file = read on restart)
pub(crate) fn indexer_endpoint(
    address: &str,
    tls: Option<&ZebraIndexerTls>,
    timeouts: Timeouts,
) -> Result<Endpoint, EndpointError> {
    let invalid = |reason: String| EndpointError::Address { address: address.to_owned(), reason };
    let plain = validator_url(address)?;
    let endpoint = match tls {
        None => Endpoint::from_shared(plain).map_err(|e| invalid(e.to_string()))?,
        Some(tls) => {
            let read = |role: &'static str, path: &Path| {
                std::fs::read(path).map_err(|source| EndpointError::TlsFile {
                    role,
                    path: path.display().to_string(),
                    source,
                })
            };
            // validator_url's host:port only (no path, no query)
            let authority = &plain["http://".len()..];
            let host = authority.rsplit_once(':').map_or(authority, |(host, _)| host);
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let config = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(read("CA", &tls.ca_path)?))
                .identity(Identity::from_pem(
                    read("certificate", &tls.cert_path)?,
                    read("key", &tls.key_path)?,
                ))
                .domain_name(tls.server_name.as_deref().unwrap_or(host));
            let endpoint = Endpoint::from_shared(format!("https://{authority}"))
                .map_err(|e| invalid(e.to_string()))?;
            endpoint.tls_config(config).map_err(|e| invalid(format!("TLS: {e}")))?
        }
    };
    Ok(endpoint.connect_timeout(timeouts.connect).tcp_nodelay(true))
}

/// One validator's indexer endpoint (`indexer_listen_addr`)
pub struct ZebraIndexerWatch {
    address: String,
    endpoint: Endpoint,
}

impl ZebraIndexerWatch {
    /// Unprobed (a validator without its indexer up is the run loop's retry)
    pub fn at(
        address: &str,
        tls: Option<&ZebraIndexerTls>,
        timeouts: Timeouts,
    ) -> Result<Self, EndpointError> {
        let endpoint = indexer_endpoint(address, tls, timeouts)?;
        Ok(Self { address: address.to_owned(), endpoint })
    }

    /// Until `cancel`: each event → `on_change`
    ///
    /// - `on_link` edges only: `true` once both streams open, `false` when either ends after
    ///   (refused connect = no edge)
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
        BlockAndHash, BlockHashAndHeight, BlockRangeRequest, BlockRequest, MempoolChangeMessage,
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

    /// - Nothing listening = refused, retried, no edge
    /// - Up: one `link true`, every event a hint
    /// - zebrad ending a lagged stream (error, then close) = one `link false`, then reconnected
    #[tokio::test]
    async fn events_wake_and_a_lagged_stream_drops_the_link_then_reconnects() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("bound").to_string();
        drop(listener);

        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let (changes, links) = (Arc::clone(&seen), Arc::clone(&seen));
        let cancel = CancellationToken::new();
        let watch = ZebraIndexerWatch::at(&address, None, Timeouts::default()).expect("address");
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

    /// zakurad's `[rpc.indexer_tls]` (mutual TLS):
    /// - a client certificate from another CA = refused in the handshake, never linked
    /// - one the server's client CA signed, server verified by our CA = linked, events flow
    /// - an unreadable PEM = refused at construction, naming the file
    #[tokio::test]
    async fn mutual_tls_links_only_with_a_client_certificate_the_server_trusts() {
        use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa};
        use rcgen::{Issuer, KeyPair};
        use tonic::transport::server::TcpIncoming;
        use tonic::transport::{Server, ServerTlsConfig};

        let dir = tempfile::tempdir().expect("tempdir");
        let write = |name: &str, pem: String| {
            let path = dir.path().join(name);
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
        let (server_cert, server_key) = leaf("server", ExtendedKeyUsagePurpose::ServerAuth, &ours);
        let (client_cert, client_key) = leaf("client", ExtendedKeyUsagePurpose::ClientAuth, &ours);
        let (stranger_cert, stranger_key) =
            leaf("stranger", ExtendedKeyUsagePurpose::ClientAuth, &theirs);

        let (tip_tx, tip_rx) = mpsc::channel(8);
        let (_pool_tx, pool_rx) = mpsc::channel(8);
        let indexer =
            FakeIndexer { tips: Mutex::new(Some(tip_rx)), mempool: Mutex::new(Some(pool_rx)) };
        let pem = |path: &PathBuf| std::fs::read(path).expect("pem");
        let server_tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(pem(&server_cert), pem(&server_key)))
            .client_ca_root(Certificate::from_pem(pem(&ca_path)));
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = socket.local_addr().expect("bound").to_string();
        let mut server = Server::builder().tls_config(server_tls).expect("server tls");
        let server = server
            .add_service(IndexerServer::new(indexer))
            .serve_with_incoming(TcpIncoming::from(socket));
        tokio::spawn(server);

        let client = |cert: &PathBuf, key: &PathBuf| ZebraIndexerTls {
            ca_path: ca_path.clone(),
            cert_path: cert.clone(),
            key_path: key.clone(),
            server_name: Some("localhost".to_owned()),
        };
        let watch = |tls: ZebraIndexerTls| {
            let (seen_tx, seen_rx) = mpsc::unbounded_channel::<String>();
            let links = seen_tx.clone();
            let cancel = CancellationToken::new();
            let watch =
                ZebraIndexerWatch::at(&address, Some(&tls), Timeouts::default()).expect("pems");
            let watching = tokio::spawn(watch.run(
                cancel.clone(),
                move |change| seen_tx.send(format!("{change:?}")).expect("test listening"),
                move |up| links.send(format!("link {up}")).expect("test listening"),
            ));
            (seen_rx, cancel, watching)
        };

        let (mut stranger_seen, cancel, watching) = watch(client(&stranger_cert, &stranger_key));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        cancel.cancel();
        watching.await.expect("only cancel ends the watch");
        let stranger = stranger_seen.try_recv().ok();
        assert_eq!(stranger, None, "another CA's client: no link, no event");

        let (mut seen, cancel, watching) = watch(client(&client_cert, &client_key));
        let in_time = Duration::from_secs(10);
        let linked = tokio::time::timeout(in_time, seen.recv()).await.expect("linked in time");
        assert_eq!(linked.as_deref(), Some("link true"), "our CA's client: linked");
        tip_tx.send(Ok(BlockHashAndHeight { hash: vec![1; 32], height: 7 })).await.expect("open");
        let tip = tokio::time::timeout(in_time, seen.recv()).await.expect("the tip in time");
        assert_eq!(tip.as_deref(), Some("Tip"), "events flow over TLS");
        cancel.cancel();
        watching.await.expect("only cancel ends the watch");

        let missing = ZebraIndexerTls {
            ca_path: dir.path().join("missing.pem"),
            ..client(&client_cert, &client_key)
        };
        let refused = ZebraIndexerWatch::at(&address, Some(&missing), Timeouts::default());
        let named = matches!(
            refused,
            Err(EndpointError::TlsFile { role: "CA", ref path, .. }) if path.ends_with("missing.pem")
        );
        assert!(named, "an unreadable PEM names itself at construction");
    }
}
