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
use zaino_primitives::display::{ByteSize, Human};
use zaino_sync::IndexKind;

use crate::config::{DaemonConfig, IndexSnapshotConfig};
use crate::error::IndexerError;
use crate::health::DaemonStatus;
use crate::telemetry::disk::disk_bytes;
use aria2::{Aria2, State};

mod aria2;

const MANIFEST: &str = "manifest.json";
const ARCHIVE: &str = "archive.tar.zst";
const UNPACKED: &str = "unpacked";
const UNPACKED_DONE: &str = "unpacked.done";

/// = the sync report's interval
const LOG_EVERY: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_secs(1);

/// Install progress = `/readyz`'s provisioning detail
///
/// - `phase` = slowest archive's, `bytes` = its bytes (archives past it whole)
/// - `bytes_per_sec` = `downloading` only
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SnapshotProgress {
    pub(crate) phase: SnapshotPhase,
    pub(crate) bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) bytes_per_sec: Option<u64>,
}

/// Declared in pipeline order (`Ord`: earliest = slowest)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotPhase {
    Downloading,
    Verifying,
    Unpacking,
}

/// Published beside the archives (`docs/running.md` "Index snapshot"): one seed, one height
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
    let mut empty = Vec::new();
    for index in config.indexes.iter() {
        if is_empty(&index.path)? {
            empty.push((index.kind, index.path.clone()));
        }
    }
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
    let board = Board::new(&fetches);
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

/// Missing or no entries (`Err` = unreadable, never taken for empty)
fn is_empty(path: &Path) -> Result<bool, IndexerError> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(at(path)(e)),
    }
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
        let dir = fetch.staging.parent().expect("staging() = parent.join(..): has a parent");
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
        zaino_sync::blocking(move || unpack(&archive, &into, read)).await?;
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
fn overall(slots: &[Slot]) -> Option<SnapshotProgress> {
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
        bytes: slots.iter().map(done).sum(),
        total_bytes: slots.iter().map(|slot| slot.bytes).sum(),
        bytes_per_sec: (phase == SnapshotPhase::Downloading).then(|| rate.sum()),
    })
}

/// Every archive's slot, folded by [`overall`] into [`DaemonStatus`] every [`POLL_EVERY`]
struct Board {
    slots: Mutex<Vec<Slot>>,
}

impl Board {
    fn new(fetches: &[Fetch]) -> Self {
        let slot = |fetch: &Fetch| {
            let (phase, done) = match fetch.unpacked() {
                true => (SnapshotPhase::Unpacking, fetch.archive.archive_bytes),
                false => (SnapshotPhase::Downloading, 0),
            };
            let step = Step::Active(phase);
            let (bytes, done) = (fetch.archive.archive_bytes, Arc::new(AtomicU64::new(done)));
            Slot { step, bytes, rate: 0, done }
        };
        Self { slots: Mutex::new(fetches.iter().map(slot).collect()) }
    }

    /// Poisoned = a panicked holder (panic hook aborts first: unreachable)
    fn slots(&self) -> MutexGuard<'_, Vec<Slot>> {
        self.slots.lock().expect("download slots mutex poisoned")
    }

    /// `slot` = an index into the fetches [`Board::new`] took
    fn set(&self, slot: usize, step: Step, done: u64, rate: u64) -> Arc<AtomicU64> {
        let mut slots = self.slots();
        let slot = &mut slots[slot];
        (slot.step, slot.rate) = (step, rate);
        slot.done.store(done, Ordering::Relaxed);
        Arc::clone(&slot.done)
    }

    /// `verifiedLength` present = aria2c hashing (else downloading)
    fn aria2(&self, slot: usize, state: State) {
        match state.verified {
            Some(verified) => self.set(slot, Step::Active(SnapshotPhase::Verifying), verified, 0),
            None => {
                self.set(slot, Step::Active(SnapshotPhase::Downloading), state.done, state.rate)
            }
        };
    }

    /// Unpack's byte counter (compressed bytes read)
    fn unpacking(&self, slot: usize) -> Arc<AtomicU64> {
        self.set(slot, Step::Active(SnapshotPhase::Unpacking), 0, 0)
    }

    fn installed(&self, slot: usize) {
        self.set(slot, Step::Installed, 0, 0);
    }

    /// Publishes until dropped (raced against the installs), logs every [`LOG_EVERY`]
    async fn report(&self, status: &DaemonStatus) {
        let mut ticks = tokio::time::interval(POLL_EVERY);
        let mut logged = Instant::now();
        loop {
            ticks.tick().await;
            let progress = overall(&self.slots());
            if let Some(progress) = progress.as_ref().filter(|_| logged.elapsed() >= LOG_EVERY) {
                logged = Instant::now();
                log(progress);
            }
            status.bootstrap(progress);
        }
    }
}

fn log(progress: &SnapshotProgress) {
    let (done, total) = (ByteSize(progress.bytes), ByteSize(progress.total_bytes));
    match progress.phase {
        SnapshotPhase::Downloading => {
            let rate = progress.bytes_per_sec.unwrap_or(0);
            info!(
                done = %done,
                total = %total,
                rate = %format_args!("{}/s", ByteSize(rate)),
                eta = %eta(progress.total_bytes.saturating_sub(progress.bytes), rate),
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
        self.read.fetch_add(
            u64::try_from(n).expect("one read's byte count fits u64"),
            Ordering::Relaxed,
        );
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
    use std::{collections::HashMap, convert::Infallible, net::SocketAddr, num::NonZeroU32};

    use http_body_util::Full;
    use hyper::body::Bytes;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::config::DaemonConfigFile;

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
        let at = |phase, bytes, bytes_per_sec| SnapshotProgress {
            phase,
            bytes,
            total_bytes: 600,
            bytes_per_sec,
        };
        // each slot(step, bytes done, bytes/s, archive bytes)
        #[rustfmt::skip]
        let cases = [
            ("all queued",
                [slot(downloading, 0, 0, 100), slot(downloading, 0, 0, 200), slot(downloading, 0, 0, 300)],
                Some(at(SnapshotPhase::Downloading, 0, Some(0)))),
            ("first unpacking while the second downloads",
                [slot(unpacking, 10, 0, 100), slot(downloading, 150, 9, 200), slot(downloading, 0, 0, 300)],
                Some(at(SnapshotPhase::Downloading, 100 + 150, Some(9)))),
            ("completedLength past the manifest's bytes",
                [slot(installed, 0, 0, 100), slot(downloading, 999, 4, 200), slot(downloading, 0, 0, 300)],
                Some(at(SnapshotPhase::Downloading, 100 + 200, Some(4)))),
            ("last hashing in aria2",
                [slot(installed, 0, 0, 100), slot(unpacking, 50, 0, 200), slot(verifying, 120, 0, 300)],
                Some(at(SnapshotPhase::Verifying, 100 + 200 + 120, None))),
            ("last unpacking",
                [slot(installed, 0, 0, 100), slot(installed, 0, 0, 200), slot(unpacking, 30, 0, 300)],
                Some(at(SnapshotPhase::Unpacking, 100 + 200 + 30, None))),
            ("all installed",
                [slot(installed, 0, 0, 100), slot(installed, 0, 0, 200), slot(installed, 0, 0, 300)],
                None),
        ];
        for (case, slots, expected) in cases {
            assert_eq!(overall(&slots), expected, "{case}");
        }
    }

    /// - Manifest v2 → one fetch per empty index (manifest-relative or absolute URL, staging =
    ///   hidden sibling)
    /// - v1 manifest / index absent from the manifest → refused, naming what is missing
    #[test]
    fn a_v2_manifest_yields_one_fetch_per_empty_index() {
        let indexes = PathBuf::from("/srv/zaino/indexes");
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
        let expected = [
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
        ];
        assert_eq!(summary, expected, "(index, archive url, staging)");
        let v1 = json!({ "archive": "a.tar.zst", "bytes": 1, "sha256": "aa", "height": 7,
                         "network": "mainnet" });
        let v1 = serde_json::from_value::<Manifest>(v1).map_err(|error| error.to_string());
        assert!(
            v1.as_ref().is_err_and(|refused| refused.contains("unknown field `archive`")),
            "v1 manifest refused: {v1:?}"
        );
        let lacking = vec![(IndexKind::TreeState, indexes.join("tree_state"))];
        let lacking = fetches(&base, BTreeMap::new(), lacking).expect_err("no tree_state archive");
        assert!(lacking.to_string().contains("manifest holds no tree_state archive"), "{lacking}");
    }

    /// Missing or entry-less dir = empty, one holding data = not, unlistable (a file) = error
    #[test]
    fn only_a_missing_or_entry_less_index_dir_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (missing, bare, held, file) = (
            dir.path().join("m"),
            dir.path().join("b"),
            dir.path().join("h"),
            dir.path().join("f"),
        );
        std::fs::create_dir(&bare).expect("bare dir");
        std::fs::create_dir(&held).expect("held dir");
        std::fs::write(held.join("manifest"), b"").expect("data");
        std::fs::write(&file, b"").expect("file");

        let empty = |path: &Path| is_empty(path).map_err(|e| e.to_string());
        assert_eq!(empty(&missing), Ok(true), "missing");
        assert_eq!(empty(&bare), Ok(true), "no entries");
        assert_eq!(empty(&held), Ok(false), "holds data");
        let refused = empty(&file);
        assert!(
            refused.as_ref().is_err_and(|e| e.contains(&file.display().to_string())),
            "unlistable: error naming it, got {refused:?}"
        );
    }

    /// Space need = per filesystem Σ archive + installed − staged bytes (unpacked = none left),
    /// refused past free space naming the directory
    #[test]
    fn free_space_is_checked_per_filesystem_less_what_staging_holds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = dir.path().join("indexes");
        let base = Url::parse("https://snapshots.example/manifest.json").expect("manifest url");
        let archive = |archive_bytes, installed_bytes| Archive {
            archive: "a.tar.zst".to_owned(),
            sha256: "aa".to_owned(),
            archive_bytes,
            installed_bytes,
        };
        let archives = BTreeMap::from([
            ("compact_block".to_owned(), archive(1000, 3000)),
            ("block_hash".to_owned(), archive(100, 400)),
        ]);
        let empty = vec![
            (IndexKind::CompactBlock, indexes.join("compact_block")),
            (IndexKind::BlockHash, indexes.join("block_hash")),
        ];
        let fetched = fetches(&base, archives, empty).expect("both archived");
        std::fs::create_dir_all(&fetched[0].staging).expect("staging");
        std::fs::write(fetched[0].staging.join(ARCHIVE), [0u8; 600]).expect("partial archive");

        let need: Vec<_> = space_needed(&fetched).expect("walks").into_values().collect();
        assert_eq!(need, [(indexes.clone(), 1000 + 3000 - 600 + 100 + 400)], "one filesystem");
        marker(&fetched[0].staging.join(UNPACKED_DONE)).expect("marker");
        let need: Vec<_> = space_needed(&fetched).expect("walks").into_values().collect();
        assert_eq!(need, [(indexes.clone(), 100 + 400)], "compact_block unpacked: none left");
        check_space(&fetched).expect("500 bytes fit");

        let huge = BTreeMap::from([("block_hash".to_owned(), archive(1 << 40, u64::MAX >> 2))]);
        let huge = fetches(&base, huge, vec![(IndexKind::BlockHash, indexes.join("block_hash"))]);
        let refused = check_space(&huge.expect("archived")).expect_err("exabytes do not fit");
        let named = format!("{}: needs", indexes.display());
        assert!(refused.to_string().contains(&named), "{refused}");
    }

    /// `files` (`name` → body) as one `.tar.zst`
    fn tar_zst(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(u64::try_from(body.len()).expect("test body fits u64"));
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, *body).expect("tar entry");
        }
        zstd::encode_all(&tar.into_inner().expect("tar")[..], 0).expect("zstd")
    }

    /// Plain HTTP/1.1 on loopback: GET `path` = `files[path]` (else 404), every path requested
    /// logged (no ranges: aria2c falls back to one connection)
    async fn serve_files(files: HashMap<&'static str, Vec<u8>>) -> (SocketAddr, Requested) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let (files, requested) = (Arc::new(files), Requested::default());
        let log = Arc::clone(&requested);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (files, log) = (Arc::clone(&files), Arc::clone(&log));
                let service = hyper::service::service_fn(move |request: hyper::Request<_>| {
                    let path = request.uri().path().to_owned();
                    log.lock().expect("log").push(path.clone());
                    let body = files.get(path.as_str()).cloned();
                    let status = if body.is_some() { 200 } else { 404 };
                    let response = hyper::Response::builder()
                        .status(status)
                        .body(Full::new(Bytes::from(body.unwrap_or_default())));
                    async move { Ok::<_, Infallible>(response.expect("a valid response")) }
                });
                let connection = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service);
                tokio::spawn(connection);
            }
        });
        (address, requested)
    }

    type Requested = Arc<Mutex<Vec<String>>>;

    /// Fresh regtest node, compact_block (+ utxo_set) on, its utxo_set archive failing its sha256
    #[tokio::test]
    async fn an_archive_failing_its_sha256_stops_startup_and_wipes_its_staging() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = dir.path().join("indexes");
        let compact_block = tar_zst(&[("MANIFEST", b"cb manifest")]);
        let utxo_set = tar_zst(&[("MANIFEST", b"vb manifest")]);
        let sha256 = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        let entry = |name: &str, bytes: &[u8], sha256: String| {
            json!({ "archive": format!("{name}.tar.zst"), "sha256": sha256,
                    "archive_bytes": bytes.len(), "installed_bytes": 1 << 20 })
        };
        let manifest = json!({
            "network": "regtest", "height": 7,
            "indexes": {
                "compact_block": entry("compact_block", &compact_block, sha256(&compact_block)),
                "utxo_set": entry("utxo_set", &utxo_set, sha256(b"not the archive")),
            },
        });
        let (server, _) = serve_files(HashMap::from([
            ("/v/manifest.json", manifest.to_string().into_bytes()),
            ("/v/compact_block.tar.zst", compact_block),
            ("/v/utxo_set.tar.zst", utxo_set),
        ]))
        .await;
        let config: DaemonConfigFile = toml::from_str(&format!(
            "network = \"regtest\"\n[index.compact_block]\npath = {:?}\n\
             [index.block_hash]\nenabled = false\n[index.tree_state]\nenabled = false\n\
             [index.transparent_address]\nenabled = false\n[index.txid_location]\nenabled = false\n",
            indexes.join("compact_block"),
        ))
        .expect("config");
        let config = config.resolve().expect("resolves");
        let snapshot = IndexSnapshotConfig {
            manifest: format!("http://{server}/v/manifest.json"),
            connections: NonZeroU32::new(4).expect("4"),
        };
        let status = DaemonStatus::new(config.network, []);

        let refused = install(&snapshot, &config, &status).await.expect_err("bad sha256");

        assert!(refused.to_string().contains("utxo_set.tar.zst"), "names the archive: {refused}");
        let staging = indexes.join(".utxo_set.snapshot");
        assert!(!staging.exists(), "bad archive's staging wiped: {}", staging.display());
    }

    /// Fresh regtest node, compact_block (+ utxo_set) and block_hash on, block_hash holding data:
    /// - both empty indexes installed byte for byte, nothing staged left
    /// - block_hash's archive never fetched, bootstrap status cleared (aria2c from the dev shell)
    #[tokio::test]
    async fn install_fills_every_empty_index_and_never_fetches_one_holding_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = dir.path().join("indexes");
        std::fs::create_dir_all(indexes.join("block_hash")).expect("block_hash dir");
        std::fs::write(indexes.join("block_hash/MANIFEST"), b"held").expect("block_hash data");
        let compact_block = tar_zst(&[("MANIFEST", b"cb manifest"), ("blocks.dat", b"cb blocks")]);
        let utxo_set = tar_zst(&[("MANIFEST", b"vb manifest")]);
        let sha256 = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        let entry = |name: &str, bytes: &[u8]| {
            json!({ "archive": format!("{name}.tar.zst"), "sha256": sha256(bytes),
                    "archive_bytes": bytes.len(), "installed_bytes": 1 << 20 })
        };
        let manifest = json!({
            "network": "regtest", "height": 7,
            "indexes": {
                "compact_block": entry("compact_block", &compact_block),
                "utxo_set": entry("utxo_set", &utxo_set),
                "block_hash": entry("block_hash", b"never"),
            },
        });
        let (server, requested) = serve_files(HashMap::from([
            ("/v/manifest.json", manifest.to_string().into_bytes()),
            ("/v/compact_block.tar.zst", compact_block),
            ("/v/utxo_set.tar.zst", utxo_set),
            ("/v/block_hash.tar.zst", b"never".to_vec()),
        ]))
        .await;
        let config: DaemonConfigFile = toml::from_str(&format!(
            "network = \"regtest\"\n[index.compact_block]\npath = {:?}\n\
             [index.block_hash]\npath = {:?}\n[index.tree_state]\nenabled = false\n\
             [index.transparent_address]\nenabled = false\n[index.txid_location]\nenabled = false\n",
            indexes.join("compact_block"),
            indexes.join("block_hash"),
        ))
        .expect("config");
        let config = config.resolve().expect("resolves");
        let snapshot = IndexSnapshotConfig {
            manifest: format!("http://{server}/v/manifest.json"),
            connections: NonZeroU32::new(4).expect("4"),
        };
        let status = DaemonStatus::new(config.network, []);

        install(&snapshot, &config, &status).await.expect("installs");

        for (file, expected) in [
            ("compact_block/MANIFEST", &b"cb manifest"[..]),
            ("compact_block/blocks.dat", b"cb blocks"),
            ("utxo_set/MANIFEST", b"vb manifest"),
            ("block_hash/MANIFEST", b"held"),
        ] {
            let installed = std::fs::read(indexes.join(file)).expect(file);
            assert_eq!(installed, expected, "{file}");
        }
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
