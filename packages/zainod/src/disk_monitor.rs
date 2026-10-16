//! What the disk is doing to zainod: one sample per progress tick, as metrics
//!
//! - per table: durable bytes + their page-cache share (cold lookups = disk reads)
//! - per index directory: its filesystem's free + capacity
//! - walk's total = [`walked`]
//! - process: block-device bytes read / written, time stalled on I/O (own cgroup's PSI)
//! - unreadable source (no PSI, not Linux) = its metric left out, never a guessed zero

use std::path::{Path, PathBuf};

use metrics::Unit;
use zaino_persistence::IndexKind;
use zaino_primitives::metric::Metric;

use crate::progress::Index;

mod names {
    pub(super) const COMMITTED_BYTES: &str = "zaino.disk.committed_bytes";
    pub(super) const CACHED_BYTES: &str = "zaino.disk.cached_bytes";
    pub(super) const BYTES: &str = "zaino.disk.bytes";
    pub(super) const FREE_BYTES: &str = "zaino.disk.free_bytes";
    pub(super) const CAPACITY_BYTES: &str = "zaino.disk.capacity_bytes";
    pub(super) const READ_BYTES_TOTAL: &str = "zaino.disk.read_bytes_total";
    pub(super) const WRITTEN_BYTES_TOTAL: &str = "zaino.disk.written_bytes_total";
    pub(super) const IO_STALL_SECONDS_TOTAL: &str = "zaino.io.stall_seconds_total";
}

pub(crate) const METRICS: &[Metric] = {
    use names::*;

    &[
        Metric::gauge(
            COMMITTED_BYTES,
            "Committed data per table (records + rows as stored), by index and table",
        )
        .unit(Unit::Bytes),
        Metric::gauge(
            CACHED_BYTES,
            "Committed bytes in page cache (rest = a disk read on first touch), by index and table",
        )
        .unit(Unit::Bytes),
        Metric::gauge(
            BYTES,
            "Index directory on disk (preallocation + uncommitted merges included), by index",
        )
        .unit(Unit::Bytes),
        Metric::gauge(FREE_BYTES, "Free space on the index's filesystem").unit(Unit::Bytes),
        Metric::gauge(CAPACITY_BYTES, "Size of the index's filesystem").unit(Unit::Bytes),
        Metric::counter(READ_BYTES_TOTAL, "Block-device bytes zainod read").unit(Unit::Bytes),
        Metric::counter(WRITTEN_BYTES_TOTAL, "Block-device bytes zainod caused to be written")
            .unit(Unit::Bytes),
        Metric::counter(
            IO_STALL_SECONDS_TOTAL,
            "Time zainod's cgroup had tasks stalled on I/O: some = at least one, full = all",
        )
        .unit(Unit::Seconds),
    ]
};

/// One sample of every index + the process (blocking: `mincore` + `statvfs` + procfs reads)
pub(crate) fn sample(indexes: &[(IndexKind, PathBuf, zaino_persistence::DiskView)]) {
    for (kind, dir, view) in indexes {
        let index = kind.name();
        for table in view.footprint() {
            let labels = [("index", index), ("table", table.table)];
            metrics::gauge!(names::COMMITTED_BYTES, &labels).set(table.bytes as f64);
            if let Some(cached) = table.cached {
                metrics::gauge!(names::CACHED_BYTES, &labels).set(cached as f64);
            }
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
    if let Some((some, full)) = own_io_pressure() {
        metrics::counter!(names::IO_STALL_SECONDS_TOTAL, "kind" => "some")
            .absolute(some / 1_000_000);
        metrics::counter!(names::IO_STALL_SECONDS_TOTAL, "kind" => "full")
            .absolute(full / 1_000_000);
    }
}

/// Each walked index directory's total
pub(crate) fn walked(index: &'static str, bytes: u64) {
    metrics::gauge!(names::BYTES, "index" => index).set(bytes as f64);
}

/// The sampled set: each index's kind, directory and durable view now
pub(crate) fn views(indexes: &[Index]) -> Vec<(IndexKind, PathBuf, zaino_persistence::DiskView)> {
    indexes
        .iter()
        .map(|index| (index.kind, index.dir.clone(), index.handle.durable_view()))
        .collect()
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

/// This process's cgroup v2 `io.pressure` totals (µs): own cgroup = the container, the systemd
/// unit, or the session
fn own_io_pressure() -> Option<(u64, u64)> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup_v2_path(&cgroup)?;
    let pressure =
        std::fs::read_to_string(Path::new("/sys/fs/cgroup").join(path).join("io.pressure"));
    pressure_totals(&pressure.ok()?)
}

/// `0::<path>` line of `/proc/self/cgroup`, relative (cgroup v2 only)
fn cgroup_v2_path(cgroup: &str) -> Option<&str> {
    cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim_start_matches('/'))
}

/// `(some, full)` `total=` µs of a PSI file
fn pressure_totals(pressure: &str) -> Option<(u64, u64)> {
    let total = |kind: &str| {
        let line = pressure.lines().find(|line| line.starts_with(kind))?;
        let total = line.split_whitespace().find_map(|field| field.strip_prefix("total="))?;
        total.parse::<u64>().ok()
    };
    Some((total("some ")?, total("full ")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_io_reads_the_block_layer_fields() {
        let io = "rchar: 64042856889\nwchar: 64042856889\nsyscr: 1\nsyscw: 2\n\
                  read_bytes: 23569625088\nwrite_bytes: 70950584320\ncancelled_write_bytes: 0\n";
        assert_eq!(proc_io(io), Some((23_569_625_088, 70_950_584_320)));
        assert_eq!(proc_io("rchar: 1\n"), None, "no block-layer fields");
    }

    #[test]
    fn cgroup_v2_path_is_the_unified_line_relative() {
        for (cgroup, path) in [
            ("0::/\n", Some("")),
            ("0::/system.slice/zainod.service\n", Some("system.slice/zainod.service")),
            ("12:cpu,cpuacct:/x\n", None),
        ] {
            assert_eq!(cgroup_v2_path(cgroup), path, "{cgroup:?}");
        }
    }

    #[test]
    fn pressure_totals_need_both_some_and_full() {
        let pressure = "some avg10=92.26 avg60=92.93 avg300=91.80 total=11954751839\n\
                        full avg10=89.02 avg60=89.51 avg300=87.84 total=9874765437\n";
        assert_eq!(pressure_totals(pressure), Some((11_954_751_839, 9_874_765_437)));
        assert_eq!(pressure_totals("some avg10=0.00 total=5\n"), None, "no full line");
    }
}
