//! Per-index disk footprint as metrics: one task, every [`SAMPLE_EVERY`]
//!
//! - committed bytes + their page-cache share (cold lookups = disk reads), tables summed
//! - directory walked ([`disk_bytes`]) every [`WALK_EVERY`] → `zaino.disk.bytes` + [`Sizes`]
//! - filesystem space + block-device I/O left to node_exporter / cAdvisor (per host, not index)

use std::{collections::BTreeMap, io, path::Path, path::PathBuf, time::Duration};

use metrics::Unit;
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use zaino_persistence::DiskView;
use zaino_primitives::metric::Metric;
use zaino_sync::IndexKind;

use super::OpenedIndex;

const SAMPLE_EVERY: Duration = Duration::from_secs(30);
const WALK_EVERY: Duration = Duration::from_secs(120);

/// Last walk's bytes per index name (unreadable = absent)
pub(crate) type Sizes = BTreeMap<&'static str, u64>;

mod names {
    pub(super) const COMMITTED_BYTES: &str = "zaino.disk.committed_bytes";
    pub(super) const CACHED_BYTES: &str = "zaino.disk.cached_bytes";
    pub(super) const BYTES: &str = "zaino.disk.bytes";
}

pub(crate) const METRICS: &[Metric] = {
    use names::*;

    &[
        Metric::gauge(COMMITTED_BYTES, "Committed data (records + rows as stored), by index")
            .unit(Unit::Bytes),
        Metric::gauge(
            CACHED_BYTES,
            "Committed bytes in page cache (rest = a disk read on first touch), by index",
        )
        .unit(Unit::Bytes),
        Metric::gauge(
            BYTES,
            "Index directory on disk (preallocation + uncommitted merges included), by index",
        )
        .unit(Unit::Bytes),
    ]
};

/// Until `cancel`: every tick a footprint sample, every [`WALK_EVERY`] a walk into `sizes`
pub(crate) async fn run(
    indexes: Vec<OpenedIndex>,
    sizes: watch::Sender<Sizes>,
    cancel: CancellationToken,
) {
    let mut ticks = tokio::time::interval(SAMPLE_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut walked = None::<Instant>;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        if walked.is_none_or(|at| at.elapsed() >= WALK_EVERY) {
            let walk = walk(&indexes).await;
            for (index, bytes) in &walk {
                metrics::gauge!(names::BYTES, "index" => *index).set(*bytes as f64);
            }
            sizes.send_replace(walk);
            walked = Some(Instant::now());
        }
        let views: Vec<_> =
            indexes.iter().map(|index| (index.kind, index.handle.durable_view())).collect();
        zaino_sync::blocking(move || sample(&views)).await;
    }
}

/// Blocking: one `mincore` pass per index
fn sample(indexes: &[(IndexKind, DiskView)]) {
    for (kind, view) in indexes {
        let index = kind.name();
        let footprint = view.footprint();
        let committed: u64 = footprint.iter().map(|table| table.bytes).sum();
        metrics::gauge!(names::COMMITTED_BYTES, "index" => index).set(committed as f64);
        // one unreadable table = no total (a partial sum would read as cache loss)
        if let Some(cached) = footprint.iter().map(|table| table.cached).sum::<Option<u64>>() {
            metrics::gauge!(names::CACHED_BYTES, "index" => index).set(cached as f64);
        }
    }
}

/// Every index directory, off the runtime (an unreadable one logged here, under its component)
async fn walk(indexes: &[OpenedIndex]) -> Sizes {
    let dirs: Vec<PathBuf> = indexes.iter().map(|index| index.dir.clone()).collect();
    let walked =
        zaino_sync::blocking(move || dirs.iter().map(|dir| disk_bytes(dir)).collect::<Vec<_>>());
    let mut sizes = Sizes::new();
    for (index, walked) in indexes.iter().zip(walked.await) {
        match walked {
            Ok(bytes) => drop(sizes.insert(index.kind.name(), bytes)),
            Err(error) => index.span.in_scope(|| warn!(%error, "Index size unreadable")),
        }
    }
    sizes
}

/// Bytes of every file under `path`, subdirectories included (plain `stat`s, no lock)
///
/// - file removed mid-walk = gone
pub(crate) fn disk_bytes(path: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let bytes = match entry.metadata() {
            Ok(meta) if meta.is_dir() => disk_bytes(&entry.path()),
            Ok(meta) => Ok(meta.len()),
            Err(error) => Err(error),
        };
        match bytes {
            Ok(bytes) => total += bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file counted, nested included (missing directory = error)
    #[test]
    fn disk_bytes_sums_every_file_under_the_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("MANIFEST"), [0; 10]).expect("manifest");
        std::fs::create_dir(root.path().join("by_hash")).expect("subdir");
        std::fs::write(root.path().join("by_hash/0.seg"), [0; 4096]).expect("segment");
        std::fs::write(root.path().join("by_hash/1.seg"), []).expect("empty segment");

        let summed = disk_bytes(root.path()).expect("walk");
        assert_eq!(summed, 10 + 4096, "MANIFEST + by_hash/0.seg + empty by_hash/1.seg");
        let missing = disk_bytes(&root.path().join("absent")).expect_err("absent");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound, "absent directory: {missing}");
    }
}
