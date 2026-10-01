//! TLS termination for the gRPC listener: rustls + ring, cert files reloaded on change
//!
//! - Provider passed per `ServerConfig`, never installed process-wide (two providers in one
//!   graph = rustls refuses to pick: zainod 0.5.0's startup panic, #1360)
//! - Renewal = new files on disk; [`Tls::reload`] swaps them in whole, a bad pair keeps the old

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use arc_swap::ArcSwap;
use rustls::{
    crypto::CryptoProvider,
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
    ServerConfig,
};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// How often the cert files are checked for a renewal
const RELOAD_EVERY: Duration = Duration::from_secs(60);

/// PEM files: the certificate chain (leaf first) and its private key
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsFiles {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Why a certificate pair could not be loaded
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("{path}: {source}")]
    Pem { path: PathBuf, source: rustls::pki_types::pem::Error },
    #[error("{0}: no certificate in the file")]
    NoCertificate(PathBuf),
    #[error("{path}: {source}")]
    Key { path: PathBuf, source: rustls::Error },
    #[error("TLS config: {0}")]
    Config(rustls::Error),
}

/// The gRPC listener's TLS: one acceptor, its certificate swappable underneath
pub struct Tls {
    acceptor: TlsAcceptor,
    certs: Arc<Certs>,
}

impl Tls {
    /// Boot-time load: a missing or mismatched pair fails startup, never serves plaintext
    pub fn load(files: TlsFiles) -> Result<Self, TlsError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let current = load(&files, &provider)?;
        let certs = Arc::new(Certs {
            seen: std::sync::Mutex::new(stamp(&files).ok()),
            current: ArcSwap::from_pointee(current),
            files,
            provider: Arc::clone(&provider),
        });
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(TlsError::Config)?
            .with_no_client_auth()
            .with_cert_resolver(Arc::clone(&certs) as Arc<dyn ResolvesServerCert>);
        // gRPC = HTTP/2 only
        config.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Self { acceptor: TlsAcceptor::from(Arc::new(config)), certs })
    }

    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }

    /// Checks the files every [`RELOAD_EVERY`] until `cancel`
    pub(crate) fn reload(
        &self,
        cancel: CancellationToken,
    ) -> impl Future<Output = ()> + Send + 'static {
        let certs = Arc::clone(&self.certs);
        async move {
            let mut tick = tokio::time::interval(RELOAD_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    _ = tick.tick() => certs.reload_if_changed(),
                }
            }
        }
    }
}

/// Every handshake's certificate source
#[derive(Debug)]
struct Certs {
    files: TlsFiles,
    provider: Arc<CryptoProvider>,
    current: ArcSwap<CertifiedKey>,
    /// Files' stamp at the last load attempt (a failed one too: no retry until they change)
    seen: std::sync::Mutex<Option<Stamp>>,
}

impl ResolvesServerCert for Certs {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.load_full())
    }
}

impl Certs {
    fn reload_if_changed(&self) {
        let Ok(now) = stamp(&self.files) else { return };
        let mut seen = self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *seen == Some(now) {
            return;
        }
        *seen = Some(now);
        match load(&self.files, &self.provider) {
            Ok(renewed) => {
                self.current.store(Arc::new(renewed));
                info!(cert = %self.files.cert_path.display(), "TLS certificate reloaded");
            }
            // e.g. cert written, key not yet: retried when the key lands (new stamp)
            Err(error) => warn!(%error, "TLS certificate reload failed; serving the previous one"),
        }
    }
}

/// (mtime, len) of cert then key
type Stamp = [(SystemTime, u64); 2];

fn stamp(files: &TlsFiles) -> io::Result<Stamp> {
    let of = |path: &Path| -> io::Result<(SystemTime, u64)> {
        let meta = std::fs::metadata(path)?;
        Ok((meta.modified()?, meta.len()))
    };
    Ok([of(&files.cert_path)?, of(&files.key_path)?])
}

fn pem(path: &Path) -> impl FnOnce(rustls::pki_types::pem::Error) -> TlsError + '_ {
    move |source| TlsError::Pem { path: path.to_owned(), source }
}

fn load(files: &TlsFiles, provider: &CryptoProvider) -> Result<CertifiedKey, TlsError> {
    let chain = CertificateDer::pem_file_iter(&files.cert_path)
        .map_err(pem(&files.cert_path))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(pem(&files.cert_path))?;
    if chain.is_empty() {
        return Err(TlsError::NoCertificate(files.cert_path.clone()));
    }
    let key = PrivateKeyDer::from_pem_file(&files.key_path).map_err(pem(&files.key_path))?;
    let key_error = |source| TlsError::Key { path: files.key_path.clone(), source };
    let signing = provider.key_provider.load_private_key(key).map_err(key_error)?;
    let certified = CertifiedKey::new(chain, signing);
    // a key from another pair = every handshake fails: refuse it here instead
    certified.keys_match().map_err(key_error)?;
    Ok(certified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::TlsConnector;

    /// Serves its pair over a real h2 handshake; a renewal swaps in without a restart; a
    /// mismatched renewal keeps the last good pair; a mismatched boot pair refuses to start
    #[tokio::test]
    async fn serves_renews_and_refuses_a_bad_pair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = TlsFiles {
            cert_path: dir.path().join("cert.pem"),
            key_path: dir.path().join("key.pem"),
        };
        let a = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert a");
        let b = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("cert b");
        // mtime set explicitly: a rewrite inside one clock tick must still read as a change
        let mut written = SystemTime::now();
        let mut write = |path: &Path, pem: String| {
            std::fs::write(path, pem).expect("write pem");
            written += Duration::from_secs(1);
            std::fs::File::options()
                .write(true)
                .open(path)
                .expect("open")
                .set_modified(written)
                .expect("mtime");
        };
        write(&files.cert_path, a.cert.pem());
        write(&files.key_path, a.signing_key.serialize_pem());

        let tls = Tls::load(files.clone()).expect("pair a loads");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor = tls.acceptor();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move { drop(acceptor.accept(socket).await) });
            }
        });
        let mut roots = RootCertStore::empty();
        roots.add(a.cert.der().clone()).expect("root a");
        roots.add(b.cert.der().clone()).expect("root b");
        let mut client =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
        client.alpn_protocols = vec![b"h2".to_vec()];
        let connector = TlsConnector::from(Arc::new(client));
        let presented = || async {
            let socket = TcpStream::connect(addr).await.expect("connect");
            let name = ServerName::try_from("localhost").expect("name");
            let stream = connector.connect(name, socket).await.expect("handshake");
            let (_, session) = stream.get_ref();
            assert_eq!(session.alpn_protocol(), Some(&b"h2"[..]), "ALPN must negotiate h2");
            session.peer_certificates().expect("server certs")[0].clone()
        };
        assert_eq!(presented().await, *a.cert.der(), "boot pair served");

        write(&files.cert_path, b.cert.pem());
        write(&files.key_path, b.signing_key.serialize_pem());
        tls.certs.reload_if_changed();
        assert_eq!(presented().await, *b.cert.der(), "renewed pair served without a restart");

        write(&files.key_path, a.signing_key.serialize_pem());
        tls.certs.reload_if_changed();
        assert_eq!(presented().await, *b.cert.der(), "mismatched renewal keeps the last good pair");

        assert!(
            matches!(Tls::load(files), Err(TlsError::Key { .. })),
            "mismatched pair refuses to boot"
        );
    }
}
