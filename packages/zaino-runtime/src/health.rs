//! The health edge — a small HTTP server exposing the runtime signals as the
//! standard k8s probes (`/livez`, `/readyz`, `/startupz`).
//!
//! It is itself a [`RunLoop`] (so it can be a supervised component like any other
//! server) and reads the runtime's [`RuntimeSignals`] `watch` per request, so
//! every probe answer reflects the orchestration. Deliberately tiny and
//! dependency-free: it hand-parses the request line and writes a status — enough
//! for an httpGet probe, no HTTP framework.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use zaino_component::{CancellationToken, Lifecycle, RunLoop, RunReporter};

use crate::signals::RuntimeSignals;

/// Why the health edge could not run.
#[derive(Debug, thiserror::Error)]
pub enum HealthServeError {
    /// Could not bind the health listener.
    #[error("health server bind failed: {0}")]
    Bind(String),
}

/// An HTTP health edge over the runtime signals.
pub struct HealthServer {
    bind: SocketAddr,
    signals: watch::Receiver<RuntimeSignals>,
}

impl HealthServer {
    /// A health edge bound to `bind`, answering from `signals`.
    pub fn new(bind: SocketAddr, signals: watch::Receiver<RuntimeSignals>) -> Self {
        Self { bind, signals }
    }
}

impl RunLoop for HealthServer {
    type Error = HealthServeError;
    const LABEL: &'static str = "serve loop";
    const RUNNING: Lifecycle = Lifecycle::Spawning;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), HealthServeError> {
        let listener = TcpListener::bind(self.bind)
            .await
            .map_err(|e| HealthServeError::Bind(e.to_string()))?;
        reporter.ready();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        // Read the latest signals per connection.
                        let signals = *self.signals.borrow();
                        tokio::spawn(handle_connection(stream, signals));
                    }
                    // A transient accept error is ignored; keep serving.
                }
            }
        }
    }
}

/// Read the request line, answer the probe, close.
async fn handle_connection(mut stream: TcpStream, signals: RuntimeSignals) {
    let mut buf = [0u8; 1024];
    let Ok(n) = stream.read(&mut buf).await else {
        return;
    };
    let request = String::from_utf8_lossy(&buf[..n]);
    // "GET /readyz HTTP/1.1" -> the path is the second token.
    let path = request.split_whitespace().nth(1).unwrap_or("");
    let (status, reason, body) = probe(path, &signals);
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

/// Map a probe path + the current signals to an HTTP status.
fn probe(path: &str, signals: &RuntimeSignals) -> (u16, &'static str, &'static str) {
    let up = match path {
        "/livez" => Some(signals.live),
        "/readyz" => Some(signals.ready),
        "/startupz" => Some(signals.started),
        _ => None,
    };
    match up {
        Some(true) => (200, "OK", "ok"),
        Some(false) => (503, "Service Unavailable", "unavailable"),
        None => (404, "Not Found", "not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::probe;
    use crate::signals::RuntimeSignals;

    const BOOTING: RuntimeSignals = RuntimeSignals {
        started: false,
        live: true,
        ready: false,
    };
    const SERVING: RuntimeSignals = RuntimeSignals {
        started: true,
        live: true,
        ready: true,
    };

    #[test]
    fn livez_is_up_while_the_process_runs() {
        assert_eq!(probe("/livez", &BOOTING).0, 200);
        assert_eq!(probe("/livez", &SERVING).0, 200);
    }

    #[test]
    fn readyz_follows_readiness() {
        assert_eq!(probe("/readyz", &BOOTING).0, 503);
        assert_eq!(probe("/readyz", &SERVING).0, 200);
    }

    #[test]
    fn startupz_follows_startup() {
        assert_eq!(probe("/startupz", &BOOTING).0, 503);
        assert_eq!(probe("/startupz", &SERVING).0, 200);
    }

    #[test]
    fn unknown_path_is_not_found() {
        assert_eq!(probe("/nope", &SERVING).0, 404);
    }
}
