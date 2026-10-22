//! aria2c child process + its JSON-RPC client (loopback, random secret)

use std::{
    io::Read,
    num::NonZeroU32,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use reqwest::Url;
use serde_json::{json, Value};

use super::{at, error, POLL_EVERY};
use crate::error::IndexerError;

/// aria2c's RPC listener coming up
const RPC_WAIT: Duration = Duration::from_secs(10);

/// Download's aria2 status: bytes `done` at `rate` bytes/s, `verified` while hashing
pub(super) struct State {
    pub(super) done: u64,
    pub(super) rate: u64,
    pub(super) verified: Option<u64>,
}

/// aria2c over JSON-RPC on loopback
///
/// - Killed when dropped (and stops itself if this process dies)
pub(super) struct Aria2 {
    child: tokio::process::Child,
    url: String,
    token: String,
    client: reqwest::Client,
}

impl Aria2 {
    /// `connections` = total: one download active at a time, the rest queued
    pub(super) async fn start(connections: NonZeroU32) -> Result<Self, IndexerError> {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .map_err(|e| error(format!("rpc port: {e}")))?
            .port();
        let secret = secret()?;
        let child = tokio::process::Command::new("aria2c")
            .arg("--enable-rpc")
            .arg(format!("--rpc-listen-port={port}"))
            .arg("--rpc-listen-all=false")
            .arg(format!("--rpc-secret={secret}"))
            .arg(format!("--stop-with-process={}", std::process::id()))
            .arg("--max-concurrent-downloads=1")
            .args(["--continue=true", "--max-tries=0", "--retry-wait=15"])
            .arg(format!("--split={connections}"))
            .arg(format!("--max-connection-per-server={connections}"))
            .args(["--min-split-size=64M", "--file-allocation=none", "--auto-file-renaming=false"])
            .args(["--allow-overwrite=true", "--console-log-level=warn", "--summary-interval=0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| error(format!("aria2c (on PATH?): {e}")))?;
        let aria2 = Self {
            child,
            url: format!("http://127.0.0.1:{port}/jsonrpc"),
            token: format!("token:{secret}"),
            client: reqwest::Client::new(),
        };
        aria2.wait_ready().await?;
        Ok(aria2)
    }

    async fn wait_ready(&self) -> Result<(), IndexerError> {
        let deadline = Instant::now() + RPC_WAIT;
        loop {
            match self.call("aria2.getVersion", vec![]).await {
                Ok(_) => return Ok(()),
                Err(e) if Instant::now() >= deadline => return Err(e),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value, IndexerError> {
        let mut all = vec![Value::String(self.token.clone())];
        all.extend(params);
        let body = json!({ "jsonrpc": "2.0", "id": "zainod", "method": method, "params": all });
        let reply: Value = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| error(format!("aria2 {method}: {e}")))?
            .bytes()
            .await
            .map_err(|e| error(format!("aria2 {method}: {e}")))
            .and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|e| error(format!("aria2 {method}: {e}")))
            })?;
        match reply.get("error") {
            Some(failure) => Err(error(format!("aria2 {method}: {failure}"))),
            None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    /// Queued behind earlier ones
    ///
    /// - `sha256` = checked by aria2c once downloaded (and on resume)
    pub(super) async fn add(
        &self,
        uri: &Url,
        dir: &Path,
        out: &str,
        sha256: Option<&str>,
    ) -> Result<Value, IndexerError> {
        std::fs::create_dir_all(dir).map_err(at(dir))?;
        let mut options = json!({ "dir": dir.display().to_string(), "out": out });
        if let Some(sha256) = sha256 {
            options["checksum"] = json!(format!("sha-256={sha256}"));
            options["check-integrity"] = json!("true");
        }
        self.call("aria2.addUri", vec![json!([uri.as_str()]), options]).await
    }

    /// `report` every [`POLL_EVERY`] until complete
    pub(super) async fn wait(
        &self,
        gid: &Value,
        uri: &Url,
        mut report: impl FnMut(State),
    ) -> Result<(), IndexerError> {
        let keys = json!([
            "status",
            "completedLength",
            "downloadSpeed",
            "verifiedLength",
            "verifyIntegrityPending",
            "errorMessage"
        ]);
        loop {
            tokio::time::sleep(POLL_EVERY).await;
            let status = self.call("aria2.tellStatus", vec![gid.clone(), keys.clone()]).await?;
            let field = |key: &str| status.get(key).and_then(Value::as_str);
            let number = |key: &str| field(key).and_then(|v| v.parse::<u64>().ok());
            let pending = field("verifyIntegrityPending") == Some("true");
            report(State {
                done: number("completedLength").unwrap_or(0),
                rate: number("downloadSpeed").unwrap_or(0),
                verified: number("verifiedLength").or(pending.then_some(0)),
            });
            match field("status").unwrap_or_default() {
                "complete" => return Ok(()),
                // - max-tries=0: transient failures retry inside aria2
                // - `error` = permanent (404, sha256 mismatch, …)
                "error" | "removed" => {
                    let why = field("errorMessage").unwrap_or_default();
                    return Err(error(format!("download {uri}: {why}")));
                }
                _ => {}
            }
        }
    }

    pub(super) async fn stop(mut self) {
        let _ = self.call("aria2.shutdown", vec![]).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

/// 128-bit RPC secret from the OS
fn secret() -> Result<String, IndexerError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut bytes))
        .map_err(|e| error(format!("rpc secret: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
