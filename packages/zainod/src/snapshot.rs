//! `[snapshot]`: empty indexes bootstrapped from a published snapshot before the indexer boots
//!
//! - aria2c does the download (segments, retries, resume) over its JSON-RPC: started on loopback
//!   with a random secret, `--stop-with-process` = this process
//! - Then sha256 against the manifest, unpack (zstd + tar) into staging, one rename per index
//! - Staging = `.zaino-snapshot` beside the compact-block index (same filesystem: renames)
//! - Markers make a restart resume at the step it died in (`archive.verified`, `unpacked.done`)

use std::{
    io::{self, Read},
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::{debug, info};
use zaino_sync::Human;

use crate::config::{DaemonConfig, SnapshotConfig, ZainoIndexConfig};
use crate::error::IndexerError;
use crate::logging::Size3;
use crate::status::{self, Phase, Snapshot};

const STAGING: &str = ".zaino-snapshot";
const MANIFEST: &str = "manifest.json";
const ARCHIVE: &str = "snapshot.tar.zst";
const UNPACKED: &str = "unpacked";
const VERIFIED: &str = "archive.verified";
const UNPACKED_DONE: &str = "unpacked.done";

/// = the sync report's interval
const LOG_EVERY: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_secs(1);
/// aria2c's RPC listener coming up
const RPC_WAIT: Duration = Duration::from_secs(10);

/// Published next to the archive (RUNBOOK "Index snapshot")
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    /// Archive URL, relative to the manifest's
    archive: String,
    bytes: u64,
    sha256: String,
    height: u32,
    network: String,
}

fn error(message: impl std::fmt::Display) -> IndexerError {
    IndexerError::Snapshot(message.to_string())
}

/// Fills every enabled index whose directory is missing or empty; a no-op once none is
pub(crate) async fn bootstrap(
    snapshot: &SnapshotConfig,
    config: &DaemonConfig,
) -> Result<(), IndexerError> {
    let empty: Vec<(&'static str, &Path)> = indexes(config)
        .into_iter()
        .filter(|(_, index)| index.enabled && is_empty(&index.path))
        .map(|(dir, index)| (dir, index.path.as_path()))
        .collect();
    if empty.is_empty() {
        debug!("Every index holds data; snapshot not needed");
        return Ok(());
    }
    let started = Instant::now();
    let staging = staging(config)?;
    std::fs::create_dir_all(&staging).map_err(|e| error(format!("{}: {e}", staging.display())))?;

    let manifest_url =
        reqwest::Url::parse(&snapshot.manifest).map_err(|e| error(format!("manifest url: {e}")))?;
    let aria2 = Aria2::start(&staging, snapshot.connections).await?;
    aria2.download(manifest_url.as_str(), MANIFEST, |_| {}).await?;
    let manifest = read_manifest(&staging.join(MANIFEST))?;
    let network = crate::config::network_name(config.network);
    if manifest.network != network {
        return Err(error(format!(
            "manifest is for {}, this daemon serves {network}",
            manifest.network
        )));
    }
    let archive_url =
        manifest_url.join(&manifest.archive).map_err(|e| error(format!("archive url: {e}")))?;
    let source = archive_url.host_str().unwrap_or_default().to_owned();
    let progress = |phase, done, total, rate| Snapshot {
        phase,
        source: source.clone(),
        height: manifest.height,
        done,
        total,
        rate,
    };

    let archive = staging.join(ARCHIVE);
    if !staging.join(VERIFIED).exists() && !staging.join(UNPACKED_DONE).exists() {
        info!(
            from = %source,
            to = manifest.height,
            size = %Size3(manifest.bytes),
            "Downloading index snapshot"
        );
        let mut logged = Instant::now();
        aria2
            .download(archive_url.as_str(), ARCHIVE, |state| {
                let total = state.total.max(manifest.bytes);
                status::snapshot(Some(progress(
                    Phase::Downloading,
                    state.done,
                    total,
                    Some(state.rate),
                )));
                if logged.elapsed() >= LOG_EVERY {
                    logged = Instant::now();
                    let left = total.saturating_sub(state.done);
                    info!(
                        from = %source,
                        to = manifest.height,
                        done = %Size3(state.done),
                        total = %Size3(total),
                        rate = %format_args!("{}/s", Size3(state.rate)),
                        eta = %eta(left, state.rate),
                        "Downloading index snapshot"
                    );
                }
            })
            .await?;
        aria2.stop().await;
        info!(size = %Size3(manifest.bytes), "Verifying index snapshot");
        let hash = tracked(Phase::Verifying, manifest.bytes, &progress, {
            let archive = archive.clone();
            move |read| sha256(&archive, read)
        })
        .await??;
        if hash != manifest.sha256.to_ascii_lowercase() {
            let _ = std::fs::remove_file(&archive);
            return Err(error(format!(
                "sha256 {hash} != manifest {} (archive removed)",
                manifest.sha256
            )));
        }
        marker(&staging.join(VERIFIED))?;
    } else {
        aria2.stop().await;
    }

    if !staging.join(UNPACKED_DONE).exists() {
        info!(size = %Size3(manifest.bytes), "Unpacking index snapshot");
        let into = staging.join(UNPACKED);
        tracked(Phase::Unpacking, manifest.bytes, &progress, {
            let archive = archive.clone();
            move |read| unpack(&archive, &into, read)
        })
        .await??;
        marker(&staging.join(UNPACKED_DONE))?;
        let _ = std::fs::remove_file(&archive);
    }

    for (dir, path) in &empty {
        install(&staging.join(UNPACKED).join(dir), path)?;
    }
    status::snapshot(None);
    std::fs::remove_dir_all(&staging).map_err(|e| error(format!("{}: {e}", staging.display())))?;
    info!(
        indexes = empty.len(),
        height = manifest.height,
        elapsed = %Human(started.elapsed()),
        "Index snapshot installed"
    );
    Ok(())
}

/// `(snapshot top-level directory, index config)` per index
fn indexes(config: &DaemonConfig) -> [(&'static str, &ZainoIndexConfig); 5] {
    let index = &config.index;
    [
        ("compact-block", &index.compact_block),
        ("value-balance", &index.value_balance),
        ("block-hash", &index.block_hash),
        ("tree-state", &index.tree_state),
        ("transparent-address", &index.transparent_address),
    ]
}

fn is_empty(path: &Path) -> bool {
    std::fs::read_dir(path).map_or(true, |mut entries| entries.next().is_none())
}

fn staging(config: &DaemonConfig) -> Result<PathBuf, IndexerError> {
    let index = &config.index.compact_block.path;
    index
        .parent()
        .map(|parent| parent.join(STAGING))
        .ok_or_else(|| error(format!("{} has no parent for staging", index.display())))
}

fn read_manifest(path: &Path) -> Result<Manifest, IndexerError> {
    let bytes = std::fs::read(path).map_err(|e| error(format!("manifest: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| error(format!("manifest: {e}")))
}

fn marker(path: &Path) -> Result<(), IndexerError> {
    std::fs::write(path, b"").map_err(|e| error(format!("{}: {e}", path.display())))
}

/// `src` (unpacked) → `index` path; an empty directory there is replaced
fn install(src: &Path, index: &Path) -> Result<(), IndexerError> {
    if !src.is_dir() {
        return Err(error(format!("snapshot holds no {}", src.display())));
    }
    if index.exists() {
        std::fs::remove_dir(index).map_err(|e| error(format!("{}: {e}", index.display())))?;
    }
    if let Some(parent) = index.parent() {
        std::fs::create_dir_all(parent).map_err(|e| error(format!("{}: {e}", parent.display())))?;
    }
    std::fs::rename(src, index).map_err(|e| {
        // EXDEV: staging sits beside compact_block; every index must share its filesystem
        error(format!(
            "{} → {}: {e} (index paths must share one filesystem)",
            src.display(),
            index.display()
        ))
    })
}

/// Blocking `work` on the pool, its byte counter published every [`POLL_EVERY`] and logged every
/// [`LOG_EVERY`]
async fn tracked<T: Send + 'static>(
    phase: Phase,
    total: u64,
    progress: &impl Fn(Phase, u64, u64, Option<u64>) -> Snapshot,
    work: impl FnOnce(Arc<AtomicU64>) -> T + Send + 'static,
) -> Result<T, IndexerError> {
    let read = Arc::new(AtomicU64::new(0));
    let mut job = tokio::task::spawn_blocking({
        let read = Arc::clone(&read);
        move || work(read)
    });
    let mut ticks = tokio::time::interval(POLL_EVERY);
    let mut logged = Instant::now();
    loop {
        tokio::select! {
            done = &mut job => return Ok(done?),
            _ = ticks.tick() => {
                let done = read.load(Ordering::Relaxed);
                status::snapshot(Some(progress(phase, done, total, None)));
                if logged.elapsed() >= LOG_EVERY {
                    logged = Instant::now();
                    info!(done = %Size3(done), total = %Size3(total), "{}", match phase {
                        Phase::Verifying => "Verifying index snapshot",
                        _ => "Unpacking index snapshot",
                    });
                }
            }
        }
    }
}

/// Reader that counts bytes into `read` (verify / unpack progress)
struct Counted<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn open(archive: &Path, read: Arc<AtomicU64>) -> Result<Counted<std::fs::File>, IndexerError> {
    let inner =
        std::fs::File::open(archive).map_err(|e| error(format!("{}: {e}", archive.display())))?;
    Ok(Counted { inner, read })
}

fn sha256(archive: &Path, read: Arc<AtomicU64>) -> Result<String, IndexerError> {
    let mut file = io::BufReader::with_capacity(1 << 20, open(archive, read)?);
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(|e| error(format!("verify: {e}")))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// zstd + tar into `into` (wiped first: a crash mid-unpack restarts it); `tar` refuses entries
/// escaping `into`
fn unpack(archive: &Path, into: &Path, read: Arc<AtomicU64>) -> Result<(), IndexerError> {
    if into.exists() {
        std::fs::remove_dir_all(into).map_err(|e| error(format!("{}: {e}", into.display())))?;
    }
    std::fs::create_dir_all(into).map_err(|e| error(format!("{}: {e}", into.display())))?;
    let file = io::BufReader::with_capacity(1 << 20, open(archive, read)?);
    let zstd = zstd::Decoder::with_buffer(file).map_err(|e| error(format!("unpack: {e}")))?;
    tar::Archive::new(zstd).unpack(into).map_err(|e| error(format!("unpack: {e}")))
}

fn eta(left: u64, rate: u64) -> Human {
    Human(match rate {
        0 => Duration::ZERO,
        rate => Duration::from_secs(left / rate),
    })
}

/// One download's aria2 status: bytes done of `total` at `rate` bytes/s
struct State {
    done: u64,
    total: u64,
    rate: u64,
}

/// aria2c over JSON-RPC on loopback; killed when dropped (and stops itself if this process dies)
struct Aria2 {
    child: tokio::process::Child,
    url: String,
    token: String,
    client: reqwest::Client,
}

impl Aria2 {
    async fn start(dir: &Path, connections: NonZeroU32) -> Result<Self, IndexerError> {
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
            .arg(format!("--dir={}", dir.display()))
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

    /// `uri` → `out` in the staging dir; `report` every [`POLL_EVERY`] until complete
    async fn download(
        &self,
        uri: &str,
        out: &str,
        mut report: impl FnMut(State),
    ) -> Result<(), IndexerError> {
        let gid = self.call("aria2.addUri", vec![json!([uri]), json!({ "out": out })]).await?;
        let keys =
            json!(["status", "totalLength", "completedLength", "downloadSpeed", "errorMessage"]);
        loop {
            tokio::time::sleep(POLL_EVERY).await;
            let status = self.call("aria2.tellStatus", vec![gid.clone(), keys.clone()]).await?;
            let field = |key: &str| status.get(key).and_then(Value::as_str).unwrap_or_default();
            let number = |key: &str| field(key).parse::<u64>().unwrap_or(0);
            report(State {
                done: number("completedLength"),
                total: number("totalLength"),
                rate: number("downloadSpeed"),
            });
            match field("status") {
                "complete" => return Ok(()),
                // max-tries=0: transient failures retry inside aria2; `error` = permanent (404, …)
                "error" | "removed" => {
                    return Err(error(format!("download {uri}: {}", field("errorMessage"))))
                }
                _ => {}
            }
        }
    }

    async fn stop(mut self) {
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
