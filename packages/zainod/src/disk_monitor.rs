//! What the disk is doing to zainod: one sample per progress tick, as metrics
//!
//! - per index (tables summed): durable bytes + their page-cache share (cold lookups = disk reads)
//! - per index directory: its filesystem's free + capacity
//! - walk's total = [`walked`] (bytes by [`disk_bytes`])
//! - process: block-device bytes read / written
//! - unreadable source (not Linux) = its metric left out, never a guessed zero

use std::{
    io,
    path::{Path, PathBuf},
};

use zaino_persistence::IndexKind;

use crate::boot::OpenedIndex;

mod names {
    pub(super) const COMMITTED_BYTES: &str = "zaino.disk.committed_bytes";
    pub(super) const CACHED_BYTES: &str = "zaino.disk.cached_bytes";
    pub(super) const BYTES: &str = "zaino.disk.bytes";
    pub(super) const FREE_BYTES: &str = "zaino.disk.free_bytes";
    pub(super) const CAPACITY_BYTES: &str = "zaino.disk.capacity_bytes";
    pub(super) const READ_BYTES_TOTAL: &str = "zaino.disk.read_bytes_total";
    pub(super) const WRITTEN_BYTES_TOTAL: &str = "zaino.disk.written_bytes_total";
}

pub(crate) fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, Unit};

    describe_gauge!(
        names::COMMITTED_BYTES,
        Unit::Bytes,
        "Committed data (records + rows as stored), by index"
    );
    describe_gauge!(
        names::CACHED_BYTES,
        Unit::Bytes,
        "Committed bytes in page cache (rest = a disk read on first touch), by index"
    );
    describe_gauge!(
        names::BYTES,
        Unit::Bytes,
        "Index directory on disk (preallocation + uncommitted merges included), by index"
    );
    describe_gauge!(names::FREE_BYTES, Unit::Bytes, "Free space on the index's filesystem");
    describe_gauge!(names::CAPACITY_BYTES, Unit::Bytes, "Size of the index's filesystem");
    describe_counter!(names::READ_BYTES_TOTAL, Unit::Bytes, "Block-device bytes zainod read");
    describe_counter!(
        names::WRITTEN_BYTES_TOTAL,
        Unit::Bytes,
        "Block-device bytes zainod caused to be written"
    );
}

/// One sample of every index + the process (blocking: `mincore` + `statvfs` + procfs reads)
pub(crate) fn sample(indexes: &[(IndexKind, PathBuf, zaino_persistence::DiskView)]) {
    for (kind, dir, view) in indexes {
        let index = kind.name();
        let footprint = view.footprint();
        let committed: u64 = footprint.iter().map(|table| table.bytes).sum();
        metrics::gauge!(names::COMMITTED_BYTES, "index" => index).set(committed as f64);
        // one unreadable table = no total (a partial sum would read as cache loss)
        if let Some(cached) = footprint.iter().map(|table| table.cached).sum::<Option<u64>>() {
            metrics::gauge!(names::CACHED_BYTES, "index" => index).set(cached as f64);
        }
        if let Some((free, capacity)) = space(dir) {
            metrics::gauge!(names::FREE_BYTES, "index" => index).set(free as f64);
            metrics::gauge!(names::CAPACITY_BYTES, "index" => index).set(capacity as f64);
        }
    }
    if let Some((read, written)) =
        std::fs::read_to_string("/proc/self/io").ok().and_then(|io| proc_io(&io))
    {
        metrics::counter!(names::READ_BYTES_TOTAL).absolute(read);
        metrics::counter!(names::WRITTEN_BYTES_TOTAL).absolute(written);
    }
}

/// Each walked index directory's total
pub(crate) fn walked(index: &'static str, bytes: u64) {
    metrics::gauge!(names::BYTES, "index" => index).set(bytes as f64);
}

/// The sampled set: each index's kind, directory and durable view now
pub(crate) fn views(
    indexes: &[OpenedIndex],
) -> Vec<(IndexKind, PathBuf, zaino_persistence::DiskView)> {
    indexes
        .iter()
        .map(|index| (index.kind, index.dir.clone(), index.handle.durable_view()))
        .collect()
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

/// `(free, capacity)` bytes of the filesystem holding `dir` (free = what zainod may still use)
fn space(dir: &Path) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(dir).ok()?;
    Some((stat.f_bavail.saturating_mul(stat.f_frsize), stat.f_blocks.saturating_mul(stat.f_frsize)))
}

/// `read_bytes`, `write_bytes` of `/proc/<pid>/io` (bytes the block layer moved for it)
fn proc_io(io: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        let line = io.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(": "))?;
        line.trim().parse::<u64>().ok()
    };
    Some((field("read_bytes")?, field("write_bytes")?))
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

        assert_eq!(disk_bytes(root.path()).expect("walk"), 4106);
        let missing = disk_bytes(&root.path().join("absent")).expect_err("absent");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn proc_io_reads_the_block_layer_fields() {
        let io = "rchar: 64042856889\nwchar: 64042856889\nsyscr: 1\nsyscw: 2\n\
                  read_bytes: 23569625088\nwrite_bytes: 70950584320\ncancelled_write_bytes: 0\n";
        assert_eq!(proc_io(io), Some((23_569_625_088, 70_950_584_320)));
        assert_eq!(proc_io("rchar: 1\n"), None, "no block-layer fields");
    }
}
