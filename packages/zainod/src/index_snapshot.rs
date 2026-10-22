//! `[snapshot]`: per-index archives installed into empty indexes before the indexer boots
//!
//! - aria2c over its JSON-RPC ([`aria2`]): every archive queued, one active at `connections`
//!   (`-j 1`), sha256 checked by aria2c itself
//! - Each archive as aria2c finishes it: unpack (zstd + tar) into staging, rename into its index
//! - Staging = `.<index>.snapshot` beside each index (same filesystem as its target)
//! - Free space checked per filesystem before any download
//! - `unpacked.done` = restart at install

use std::{
    collections::BTreeMap,
    io::{self, Read},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use futures::future::try_join_all;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info};
use zaino_persistence::IndexKind;
use zaino_primitives::display::{ByteSize, Human};

use crate::config::{DaemonConfig, IndexSnapshotConfig};
use crate::disk_monitor::disk_bytes;
use crate::error::IndexerError;
use crate::status::DaemonStatus;
use aria2::{Aria2, State};

mod aria2;

const MANIFEST: &str = "manifest.json";
const ARCHIVE: &str = "archive.tar.zst";
const UNPACKED: &str = "unpacked";
const UNPACKED_DONE: &str = "unpacked.done";

/// = the sync report's interval
const LOG_EVERY: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_secs(1);

/// Install progress, read by [`DaemonStatus`] before the indexer boots
///
/// - `source` = archive host, `indexes` archives fetched, `total` = Σ their bytes
/// - `phase` = slowest archive's, `done` = its bytes (archives past it whole)
/// - `rate` = bytes/s (`downloading` only)
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SnapshotProgress {
    pub(crate) phase: SnapshotPhase,
    source: String,
    height: u32,
    indexes: usize,
    pub(crate) done: u64,
    pub(crate) total: u64,
    pub(crate) rate: Option<u64>,
}

/// Declared in pipeline order (`Ord`: earliest = slowest)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotPhase {
    Downloading,
    Verifying,
    Unpacking,
}

/// Published beside the archives (`usage.md` "Index snapshot"): one seed, one height
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    network: String,
    height: u32,
    indexes: BTreeMap<String, Archive>,
}

/// Keyed by `IndexKind::name()` (`archive` = URL relative to the manifest's)
#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Archive {
    archive: String,
    sha256: String,
    archive_bytes: u64,
    installed_bytes: u64,
}

/// One empty enabled index ⨝ its archive
#[derive(Debug)]
struct Fetch {
    kind: IndexKind,
    path: PathBuf,
    staging: PathBuf,
    url: Url,
    archive: Archive,
}

impl Fetch {
    fn unpacked(&self) -> bool {
        self.staging.join(UNPACKED_DONE).exists()
    }
}

fn error(message: impl std::fmt::Display) -> IndexerError {
    IndexerError::Bootstrap(message.to_string())
}

/// `map_err` adapter: failure → `<path>: <error>`
fn at<E: std::fmt::Display>(path: &Path) -> impl FnOnce(E) -> IndexerError + '_ {
    move |e| error(format!("{}: {e}", path.display()))
}

/// Fills every enabled index whose directory is missing or empty (no-op once none is)
pub(crate) async fn install(
    snapshot: &IndexSnapshotConfig,
    config: &DaemonConfig,
    status: &DaemonStatus,
) -> Result<(), IndexerError> {
    let empty: Vec<(IndexKind, PathBuf)> = IndexKind::ALL
        .into_iter()
        .filter_map(|kind| Some((kind, config.enabled(kind)?.path)))
        .filter(|(_, path)| is_empty(path))
        .collect();
    let Some((first_kind, first_path)) = empty.first() else {
        debug!("Every index holds data; snapshot not needed");
        return Ok(());
    };
    let started = Instant::now();
    let manifest_url =
        Url::parse(&snapshot.manifest).map_err(|e| error(format!("manifest url: {e}")))?;
    let aria2 = Aria2::start(snapshot.connections).await?;
    let manifest = read_manifest(&aria2, &manifest_url, &staging(first_path, *first_kind)?).await?;
    let network = crate::config::network_name(config.network);
    if manifest.network != network {
        return Err(error(format!(
            "manifest is for {}, this daemon serves {network}",
            manifest.network
        )));
    }
    let fetches = fetches(&manifest_url, manifest.indexes, empty)?;
    check_space(&fetches)?;

    let source = fetches.first().and_then(|f| f.url.host_str()).unwrap_or_default().to_owned();
    let size: u64 = fetches.iter().map(|f| f.archive.archive_bytes).sum();
    info!(
        from = %source,
        to = manifest.height,
        indexes = fetches.len(),
        size = %ByteSize(size),
        "Downloading index snapshot"
    );
    let mut queued = Vec::with_capacity(fetches.len());
    for fetch in &fetches {
        queued.push(match fetch.unpacked() {
            true => None,
            false => {
                let sha256 = Some(fetch.archive.sha256.as_str());
                Some(aria2.add(&fetch.url, &fetch.staging, ARCHIVE, sha256).await?)
            }
        });
    }
    let board = Board::new(&fetches, source, manifest.height);
    let installs = fetches
        .iter()
        .zip(queued)
        .enumerate()
        .map(|(slot, (fetch, gid))| install_archive(&aria2, &board, slot, fetch, gid));
    tokio::select! {
        installed = try_join_all(installs) => drop(installed?),
        () = board.report(status) => {}
    }
    aria2.stop().await;
    status.bootstrap(None);
    info!(
        indexes = fetches.len(),
        height = manifest.height,
        elapsed = %Human(started.elapsed()),
        "Index snapshot installed"
    );
    Ok(())
}

fn is_empty(path: &Path) -> bool {
    std::fs::read_dir(path).map_or(true, |mut entries| entries.next().is_none())
}

/// Hidden sibling of the index dir (rename into place never crosses a filesystem)
fn staging(index: &Path, kind: IndexKind) -> Result<PathBuf, IndexerError> {
    let parent = index.parent().filter(|parent| !parent.as_os_str().is_empty());
    let parent = parent.ok_or_else(|| error(format!("{} has no parent", index.display())))?;
    Ok(parent.join(format!(".{}.snapshot", kind.name())))
}

/// Fetched by aria2c too (HTTPS without a TLS stack here), stale copy removed first
async fn read_manifest(aria2: &Aria2, url: &Url, dir: &Path) -> Result<Manifest, IndexerError> {
    let path = dir.join(MANIFEST);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join(format!("{MANIFEST}.aria2")));
    let gid = aria2.add(url, dir, MANIFEST, None).await?;
    aria2.wait(&gid, url, |_| {}).await?;
    let bytes = std::fs::read(&path).map_err(|e| error(format!("manifest: {e}")))?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice(&bytes).map_err(|e| error(format!("manifest: {e}")))
}

/// Each empty index ⨝ its archive (one absent from the manifest = refused)
fn fetches(
    base: &Url,
    mut archives: BTreeMap<String, Archive>,
    empty: Vec<(IndexKind, PathBuf)>,
) -> Result<Vec<Fetch>, IndexerError> {
    let fetch = |(kind, path): (IndexKind, PathBuf)| {
        let archive = archives
            .remove(kind.name())
            .ok_or_else(|| error(format!("manifest holds no {} archive", kind.name())))?;
        let url = base.join(&archive.archive).map_err(|e| error(format!("archive url: {e}")))?;
        Ok(Fetch { kind, staging: staging(&path, kind)?, path, url, archive })
    };
    empty.into_iter().map(fetch).collect()
}

/// Bytes still to land per filesystem (by device): archive + unpacked, less what staging holds
/// - Σ, not peak: queued archives can pile up behind a slow unpack
fn space_needed(fetches: &[Fetch]) -> Result<BTreeMap<u64, (PathBuf, u64)>, IndexerError> {
    let mut need = BTreeMap::new();
    for fetch in fetches.iter().filter(|fetch| !fetch.unpacked()) {
        let dir = fetch.staging.parent().ok_or_else(|| error("staging has no parent"))?;
        std::fs::create_dir_all(dir).map_err(at(dir))?;
        let dev = std::fs::metadata(dir).map_err(at(dir))?;
        let staged = disk_bytes(&fetch.staging).unwrap_or(0);
        let bytes = fetch.archive.archive_bytes + fetch.archive.installed_bytes;
        need.entry(dev.dev()).or_insert_with(|| (dir.to_owned(), 0)).1 +=
            bytes.saturating_sub(staged);
    }
    Ok(need)
}

/// Refused before any download (`f_bavail`: root's reserve is not ours)
fn check_space(fetches: &[Fetch]) -> Result<(), IndexerError> {
    for (dir, bytes) in space_needed(fetches)?.into_values() {
        let stat = rustix::fs::statvfs(&dir).map_err(at(&dir))?;
        let free = stat.f_bavail.saturating_mul(stat.f_frsize);
        if bytes > free {
            return Err(error(format!(
                "{}: needs {} free, {} available",
                dir.display(),
                ByteSize(bytes),
                ByteSize(free)
            )));
        }
    }
    Ok(())
}

/// One archive: aria2c's download + sha256 → unpack into staging → rename into its index
async fn install_archive(
    aria2: &Aria2,
    board: &Board,
    slot: usize,
    fetch: &Fetch,
    gid: Option<Value>,
) -> Result<(), IndexerError> {
    if let Some(gid) = gid {
        let downloaded = aria2.wait(&gid, &fetch.url, |state| board.aria2(slot, state)).await;
        if downloaded.is_err() {
            // sha256 mismatch / permanent error leaves the archive + its `.aria2` behind
            let _ = std::fs::remove_dir_all(&fetch.staging);
        }
        downloaded?;
        let read = board.unpacking(slot);
        let (archive, into) = (fetch.staging.join(ARCHIVE), fetch.staging.join(UNPACKED));
        tokio::task::spawn_blocking(move || unpack(&archive, &into, read)).await??;
        marker(&fetch.staging.join(UNPACKED_DONE))?;
        let _ = std::fs::remove_file(fetch.staging.join(ARCHIVE));
    }
    rename_into(&fetch.staging.join(UNPACKED), &fetch.path)?;
    std::fs::remove_dir_all(&fetch.staging).map_err(at(&fetch.staging))?;
    board.installed(slot);
    debug!(index = fetch.kind.name(), "Index snapshot installed");
    Ok(())
}

fn marker(path: &Path) -> Result<(), IndexerError> {
    std::fs::write(path, b"").map_err(at(path))
}

/// `src` (unpacked) → `index` path (empty directory there replaced)
fn rename_into(src: &Path, index: &Path) -> Result<(), IndexerError> {
    if index.exists() {
        std::fs::remove_dir(index).map_err(at(index))?;
    }
    std::fs::rename(src, index)
        .map_err(|e| error(format!("{} → {}: {e}", src.display(), index.display())))
}

/// Per-archive step, slowest active one = the logged phase
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Active(SnapshotPhase),
    Installed,
}

/// `done` = shared with unpack's byte counter ([`Counted`])
#[derive(Debug)]
struct Slot {
    step: Step,
    bytes: u64,
    rate: u64,
    done: Arc<AtomicU64>,
}

/// Slowest step's bytes (archives past it counted whole)
///
/// - `None` = every archive installed
fn overall(slots: &[Slot], source: &str, height: u32) -> Option<SnapshotProgress> {
    let slowest = slots.iter().map(|slot| slot.step).min()?;
    let Step::Active(phase) = slowest else { return None };
    let done = |slot: &Slot| match slot.step > slowest {
        true => slot.bytes,
        false => slot.done.load(Ordering::Relaxed).min(slot.bytes),
    };
    let downloading = |slot: &&Slot| slot.step == Step::Active(SnapshotPhase::Downloading);
    let rate = slots.iter().filter(downloading).map(|slot| slot.rate);
    Some(SnapshotProgress {
        phase,
        source: source.to_owned(),
        height,
        indexes: slots.len(),
        done: slots.iter().map(done).sum(),
        total: slots.iter().map(|slot| slot.bytes).sum(),
        rate: (phase == SnapshotPhase::Downloading).then(|| rate.sum()),
    })
}

/// Every archive's slot, folded by [`overall`] into [`DaemonStatus`] every [`POLL_EVERY`]
struct Board {
    source: String,
    height: u32,
    slots: Mutex<Vec<Slot>>,
}

impl Board {
    fn new(fetches: &[Fetch], source: String, height: u32) -> Self {
        let slot = |fetch: &Fetch| {
            let (phase, done) = match fetch.unpacked() {
                true => (SnapshotPhase::Unpacking, fetch.archive.archive_bytes),
                false => (SnapshotPhase::Downloading, 0),
            };
            let step = Step::Active(phase);
            let (bytes, done) = (fetch.archive.archive_bytes, Arc::new(AtomicU64::new(done)));
            Slot { step, bytes, rate: 0, done }
        };
        Self { source, height, slots: Mutex::new(fetches.iter().map(slot).collect()) }
    }

    /// Poison-tolerant: plain data, every update a field swap
    fn slots(&self) -> MutexGuard<'_, Vec<Slot>> {
        self.slots.lock().expect("download slots mutex poisoned")
    }

    fn set(&self, slot: usize, step: Step, done: u64, rate: u64) -> Option<Arc<AtomicU64>> {
        let mut slots = self.slots();
        let slot = slots.get_mut(slot)?;
        (slot.step, slot.rate) = (step, rate);
        slot.done.store(done, Ordering::Relaxed);
        Some(Arc::clone(&slot.done))
    }

    /// `verifiedLength` present = aria2c hashing (else downloading)
    fn aria2(&self, slot: usize, state: State) {
        let _ = match state.verified {
            Some(verified) => self.set(slot, Step::Active(SnapshotPhase::Verifying), verified, 0),
            None => {
                self.set(slot, Step::Active(SnapshotPhase::Downloading), state.done, state.rate)
            }
        };
    }

    /// Unpack's byte counter (compressed bytes read)
    fn unpacking(&self, slot: usize) -> Arc<AtomicU64> {
        self.set(slot, Step::Active(SnapshotPhase::Unpacking), 0, 0).unwrap_or_default()
    }

    fn installed(&self, slot: usize) {
        let _ = self.set(slot, Step::Installed, 0, 0);
    }

    /// Publishes until dropped (raced against the installs), logs every [`LOG_EVERY`]
    async fn report(&self, status: &DaemonStatus) {
        let mut ticks = tokio::time::interval(POLL_EVERY);
        let mut logged = Instant::now();
        loop {
            ticks.tick().await;
            let progress = overall(&self.slots(), &self.source, self.height);
            if let Some(progress) = progress.as_ref().filter(|_| logged.elapsed() >= LOG_EVERY) {
                logged = Instant::now();
                log(progress);
            }
            status.bootstrap(progress);
        }
    }
}

fn log(progress: &SnapshotProgress) {
    let (done, total) = (ByteSize(progress.done), ByteSize(progress.total));
    match progress.phase {
        SnapshotPhase::Downloading => {
            let rate = progress.rate.unwrap_or(0);
            info!(
                from = %progress.source,
                to = progress.height,
                indexes = progress.indexes,
                done = %done,
                total = %total,
                rate = %format_args!("{}/s", ByteSize(rate)),
                eta = %eta(progress.total.saturating_sub(progress.done), rate),
                "Downloading index snapshot"
            )
        }
        SnapshotPhase::Verifying => info!(done = %done, total = %total, "Verifying index snapshot"),
        SnapshotPhase::Unpacking => info!(done = %done, total = %total, "Unpacking index snapshot"),
    }
}

/// Reader that counts bytes into `read` (unpack progress)
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

/// zstd + tar into `into` (wiped first: crash mid-unpack restarts it)
///
/// - `tar` refuses entries escaping `into`
fn unpack(archive: &Path, into: &Path, read: Arc<AtomicU64>) -> Result<(), IndexerError> {
    if into.exists() {
        std::fs::remove_dir_all(into).map_err(at(into))?;
    }
    std::fs::create_dir_all(into).map_err(at(into))?;
    let file = std::fs::File::open(archive).map_err(at(archive))?;
    let file = io::BufReader::with_capacity(1 << 20, Counted { inner: file, read });
    let zstd = zstd::Decoder::with_buffer(file).map_err(|e| error(format!("unpack: {e}")))?;
    tar::Archive::new(zstd).unpack(into).map_err(|e| error(format!("unpack: {e}")))
}

fn eta(left: u64, rate: u64) -> Human {
    Human(match rate {
        0 => Duration::ZERO,
        rate => Duration::from_secs(left / rate),
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, num::NonZeroU32};

    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Three archives (100 / 200 / 300 bytes) through one install: phase = slowest archive's,
    /// archives past it counted whole, aria2's overshoot capped, rate only while downloading,
    /// `None` once every archive is installed
    #[test]
    fn progress_folds_every_archive_into_the_slowest_phase() {
        let (downloading, verifying, unpacking, installed) = (
            Step::Active(SnapshotPhase::Downloading),
            Step::Active(SnapshotPhase::Verifying),
            Step::Active(SnapshotPhase::Unpacking),
            Step::Installed,
        );
        let slot = |step, done, rate, bytes| Slot {
            step,
            bytes,
            rate,
            done: Arc::new(AtomicU64::new(done)),
        };
        let at = |phase, done, rate| SnapshotProgress {
            phase,
            source: "snapshots.example".to_owned(),
            height: 7,
            indexes: 3,
            done,
            total: 600,
            rate,
        };
        let cases = [
            (
                "all queued",
                [
                    slot(downloading, 0, 0, 100),
                    slot(downloading, 0, 0, 200),
                    slot(downloading, 0, 0, 300),
                ],
                Some(at(SnapshotPhase::Downloading, 0, Some(0))),
            ),
            (
                "first unpacking while the second downloads",
                [
                    slot(unpacking, 10, 0, 100),
                    slot(downloading, 150, 9, 200),
                    slot(downloading, 0, 0, 300),
                ],
                Some(at(SnapshotPhase::Downloading, 100 + 150, Some(9))),
            ),
            (
                "completedLength past the manifest's bytes",
                [
                    slot(installed, 0, 0, 100),
                    slot(downloading, 999, 4, 200),
                    slot(downloading, 0, 0, 300),
                ],
                Some(at(SnapshotPhase::Downloading, 100 + 200, Some(4))),
            ),
            (
                "last hashing in aria2",
                [
                    slot(installed, 0, 0, 100),
                    slot(unpacking, 50, 0, 200),
                    slot(verifying, 120, 0, 300),
                ],
                Some(at(SnapshotPhase::Verifying, 100 + 200 + 120, None)),
            ),
            (
                "last unpacking",
                [
                    slot(installed, 0, 0, 100),
                    slot(installed, 0, 0, 200),
                    slot(unpacking, 30, 0, 300),
                ],
                Some(at(SnapshotPhase::Unpacking, 100 + 200 + 30, None)),
            ),
            (
                "all installed",
                [
                    slot(installed, 0, 0, 100),
                    slot(installed, 0, 0, 200),
                    slot(installed, 0, 0, 300),
                ],
                None,
            ),
        ];
        for (case, slots, expected) in cases {
            assert_eq!(overall(&slots, "snapshots.example", 7), expected, "{case}");
        }
    }

    /// - Manifest v2 → one fetch per empty index (manifest-relative URL, staging = hidden sibling)
    /// - v1 manifest / index absent from the manifest → refused
    /// - Space need = per filesystem Σ archive + installed − staged bytes (unpacked = none left),
    ///   refused past free space
    #[test]
    fn manifest_fetches_and_free_space_per_filesystem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = dir.path().join("indexes");
        let base = Url::parse("https://snapshots.example/zaino-snapshot-1.1.0/manifest.json")
            .expect("manifest url");
        let manifest: Manifest = serde_json::from_value(json!({
            "network": "mainnet", "height": 7,
            "indexes": {
                "compact_block": { "archive": "compact_block.tar.zst", "sha256": "aa",
                                   "archive_bytes": 1000, "installed_bytes": 3000 },
                "block_hash": { "archive": "https://mirror.example/bh.tar.zst", "sha256": "bb",
                                "archive_bytes": 100, "installed_bytes": 400 },
            },
        }))
        .expect("v2 manifest");
        let empty = vec![
            (IndexKind::CompactBlock, indexes.join("compact_block")),
            (IndexKind::BlockHash, indexes.join("block_hash")),
        ];
        let fetched = fetches(&base, manifest.indexes, empty).expect("both archived");
        let summary: Vec<_> =
            fetched.iter().map(|f| (f.kind, f.url.as_str(), f.staging.clone())).collect();
        assert_eq!(
            summary,
            [
                (
                    IndexKind::CompactBlock,
                    "https://snapshots.example/zaino-snapshot-1.1.0/compact_block.tar.zst",
                    indexes.join(".compact_block.snapshot"),
                ),
                (
                    IndexKind::BlockHash,
                    "https://mirror.example/bh.tar.zst",
                    indexes.join(".block_hash.snapshot"),
                ),
            ]
        );
        let v1 = json!({ "archive": "a.tar.zst", "bytes": 1, "sha256": "aa", "height": 7,
                         "network": "mainnet" });
        assert!(serde_json::from_value::<Manifest>(v1).is_err(), "v1 manifest refused");
        let lacking = vec![(IndexKind::TreeState, indexes.join("tree_state"))];
        let lacking = fetches(&base, BTreeMap::new(), lacking).expect_err("no tree_state archive");
        assert!(lacking.to_string().contains("manifest holds no tree_state archive"), "{lacking}");

        std::fs::create_dir_all(&fetched[0].staging).expect("staging");
        std::fs::write(fetched[0].staging.join(ARCHIVE), [0u8; 600]).expect("partial archive");
        let need: Vec<_> = space_needed(&fetched).expect("walks").into_values().collect();
        assert_eq!(need, [(indexes.clone(), 1000 + 3000 - 600 + 100 + 400)], "one filesystem");
        marker(&fetched[0].staging.join(UNPACKED_DONE)).expect("marker");
        let need: Vec<_> = space_needed(&fetched).expect("walks").into_values().collect();
        assert_eq!(need, [(indexes.clone(), 100 + 400)], "unpacked = nothing left to land");
        check_space(&fetched).expect("500 bytes fit");

        let huge = Archive {
            archive: "bh.tar.zst".to_owned(),
            sha256: "bb".to_owned(),
            archive_bytes: 1 << 40,
            installed_bytes: u64::MAX >> 2,
        };
        let huge = fetches(
            &base,
            BTreeMap::from([("block_hash".to_owned(), huge)]),
            vec![(IndexKind::BlockHash, indexes.join("block_hash"))],
        )
        .expect("archived");
        let refused = check_space(&huge).expect_err("exabytes do not fit");
        assert!(
            refused.to_string().contains(&format!("{}: needs", indexes.display())),
            "{refused}"
        );
    }

    /// Fresh regtest node, compact_block (+ utxo_set) and block_hash on, block_hash holding data
    ///
    /// - utxo_set archive failing its sha256 → startup stopped, its staging wiped
    /// - Republished → restart installs both empty indexes byte for byte, block_hash's archive
    ///   never fetched, nothing staged, bootstrap status cleared (aria2c from the dev shell)
    #[tokio::test]
    async fn install_refuses_a_bad_archive_then_fills_every_empty_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = dir.path().join("indexes");
        std::fs::create_dir_all(indexes.join("block_hash")).expect("block_hash dir");
        std::fs::write(indexes.join("block_hash/MANIFEST"), b"held").expect("block_hash data");

        let archive = |files: &[(&str, &[u8])]| {
            let mut tar = tar::Builder::new(Vec::new());
            for (name, body) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, name, *body).expect("tar entry");
            }
            zstd::encode_all(&tar.into_inner().expect("tar")[..], 0).expect("zstd")
        };
        let compact_block = archive(&[("MANIFEST", b"cb manifest"), ("blocks.dat", b"cb blocks")]);
        let utxo_set = archive(&[("MANIFEST", b"vb manifest")]);
        let sha256 = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        let manifest = |utxo_set_sha256: String| {
            let entry = |name: &str, bytes: &[u8], sha256: String| {
                json!({ "archive": format!("{name}.tar.zst"), "sha256": sha256,
                        "archive_bytes": bytes.len(), "installed_bytes": 1 << 20 })
            };
            json!({
                "network": "regtest", "height": 7,
                "indexes": {
                    "compact_block": entry("compact_block", &compact_block, sha256(&compact_block)),
                    "utxo_set": entry("utxo_set", &utxo_set, utxo_set_sha256),
                    "block_hash": entry("block_hash", b"never", sha256(b"never")),
                },
            })
            .to_string()
            .into_bytes()
        };

        // Plain HTTP/1.1, whole body per GET (no ranges: aria2c falls back to one connection)
        let served: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::from([
            ("/v/manifest.json".to_owned(), manifest(sha256(b"not the archive"))),
            ("/v/compact_block.tar.zst".to_owned(), compact_block.clone()),
            ("/v/utxo_set.tar.zst".to_owned(), utxo_set.clone()),
            ("/v/block_hash.tar.zst".to_owned(), b"never".to_vec()),
        ])));
        let requested: Arc<Mutex<Vec<String>>> = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (files, log) = (Arc::clone(&served), Arc::clone(&requested));
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (files, log) = (Arc::clone(&files), Arc::clone(&log));
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let path = request.split_whitespace().nth(1).unwrap_or_default().to_owned();
                    log.lock().expect("log").push(path.clone());
                    let body = files.lock().expect("files").get(&path).cloned();
                    let head = match &body {
                        Some(body) => {
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", body.len())
                        }
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n".to_owned(),
                    };
                    let head = format!("{head}Connection: close\r\n\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body.unwrap_or_default()).await;
                });
            }
        });

        let config: DaemonConfig = toml::from_str(&format!(
            "network = \"regtest\"\n\
             [index.compact_block]\npath = \"{cb}\"\n\
             [index.block_hash]\npath = \"{bh}\"\n\
             [index.tree_state]\nenabled = false\n\
             [index.transparent_address]\nenabled = false\n\
             [index.txid_location]\nenabled = false\n",
            cb = indexes.join("compact_block").display(),
            bh = indexes.join("block_hash").display(),
        ))
        .expect("config");
        let snapshot = IndexSnapshotConfig {
            manifest: format!("http://127.0.0.1:{port}/v/manifest.json"),
            connections: NonZeroU32::new(4).expect("4"),
        };

        let status = DaemonStatus::new(config.network, []);
        let refused = install(&snapshot, &config, &status).await.expect_err("bad sha256");
        assert!(refused.to_string().contains("utxo_set.tar.zst"), "{refused}");
        assert!(!indexes.join(".utxo_set.snapshot").exists(), "bad archive wiped");

        served
            .lock()
            .expect("files")
            .insert("/v/manifest.json".to_owned(), manifest(sha256(&utxo_set)));
        install(&snapshot, &config, &status).await.expect("installs");

        let read = |path: &str| std::fs::read(indexes.join(path)).expect(path);
        let installed = [
            ("compact_block/MANIFEST", read("compact_block/MANIFEST")),
            ("compact_block/blocks.dat", read("compact_block/blocks.dat")),
            ("utxo_set/MANIFEST", read("utxo_set/MANIFEST")),
            ("block_hash/MANIFEST", read("block_hash/MANIFEST")),
        ];
        assert_eq!(
            installed,
            [
                ("compact_block/MANIFEST", b"cb manifest".to_vec()),
                ("compact_block/blocks.dat", b"cb blocks".to_vec()),
                ("utxo_set/MANIFEST", b"vb manifest".to_vec()),
                ("block_hash/MANIFEST", b"held".to_vec()),
            ]
        );
        let mut left: Vec<_> = std::fs::read_dir(&indexes)
            .expect("indexes dir")
            .map(|entry| entry.expect("entry").file_name().into_string().expect("utf-8"))
            .collect();
        left.sort();
        assert_eq!(left, ["block_hash", "compact_block", "utxo_set"], "no staging left");
        let requested = requested.lock().expect("log").clone();
        assert!(!requested.iter().any(|path| path.contains("block_hash")), "{requested:?}");
        assert_eq!(status.current_bootstrap(), None, "progress cleared");
    }
}
