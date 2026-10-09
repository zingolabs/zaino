//! What the disk is doing to zainod: one sample per progress tick, as metrics
//!
//! - per table: committed bytes + their page-cache share (cold lookups = disk reads)
//! - per index directory: its filesystem's free + capacity; the walk's total = [`walked`]
//! - process: block-device bytes read / written, time stalled on I/O (own cgroup's PSI)
//! - unreadable source (no PSI, not Linux) = its metric left out, never a guessed zero

use std::path::{Path, PathBuf};

use zaino_persistence::IndexKind;

use crate::progress::{Index, Usage};

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

pub(crate) fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, Unit};

    describe_gauge!(
        names::COMMITTED_BYTES,
        Unit::Bytes,
        "Committed data per table (records + rows as stored), by index and table"
    );
    describe_gauge!(
        names::CACHED_BYTES,
        Unit::Bytes,
        "Committed bytes in page cache (rest = a disk read on first touch), by index and table"
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
    describe_counter!(
        names::IO_STALL_SECONDS_TOTAL,
        Unit::Seconds,
        "Time zainod's cgroup had tasks stalled on I/O: some = at least one, full = all"
    );
}

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
pub(crate) fn walked(index: &'static str, usage: &Usage) {
    metrics::gauge!(names::BYTES, "index" => index).set(usage.size_bytes as f64);
}

/// The sampled set: each index's kind, directory and committed view now
pub(crate) fn views(indexes: &[Index]) -> Vec<(IndexKind, PathBuf, zaino_persistence::DiskView)> {
    indexes.iter().map(|index| (index.kind, index.dir.clone(), index.handle.view())).collect()
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

    /// procfs + cgroup formats as the kernel writes them; missing fields = no reading
    #[test]
    fn proc_io_cgroup_path_and_pressure_totals_parse_the_kernels_formats() {
        let io = "rchar: 64042856889\nwchar: 64042856889\nsyscr: 1\nsyscw: 2\n\
                  read_bytes: 23569625088\nwrite_bytes: 70950584320\ncancelled_write_bytes: 0\n";
        assert_eq!(proc_io(io), Some((23_569_625_088, 70_950_584_320)));
        assert_eq!(proc_io("rchar: 1\n"), None, "no block-layer fields");

        assert_eq!(cgroup_v2_path("0::/\n"), Some(""), "own cgroup namespace (container)");
        let unit = "0::/system.slice/zainod.service\n";
        assert_eq!(cgroup_v2_path(unit), Some("system.slice/zainod.service"));
        assert_eq!(cgroup_v2_path("12:cpu,cpuacct:/x\n"), None, "cgroup v1 only");

        let pressure = "some avg10=92.26 avg60=92.93 avg300=91.80 total=11954751839\n\
                        full avg10=89.02 avg60=89.51 avg300=87.84 total=9874765437\n";
        assert_eq!(pressure_totals(pressure), Some((11_954_751_839, 9_874_765_437)));
        assert_eq!(pressure_totals("some avg10=0.00 total=5\n"), None, "no full line");
    }
}
