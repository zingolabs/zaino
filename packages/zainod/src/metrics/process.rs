use std::sync::LazyLock;

use metrics::{counter, describe_counter, describe_gauge, gauge, Unit};
use procfs::prelude::*;
use procfs::process::{LimitValue, Process};

/// Total user and system CPU time, the only process metric that is a counter.
const CPU_SECONDS_TOTAL: &str = "process_cpu_seconds_total";
/// Open file descriptors.
const OPEN_FDS: &str = "process_open_fds";
/// The open file descriptor limit, 0 when unlimited.
const MAX_FDS: &str = "process_max_fds";
/// Virtual memory size.
const VIRTUAL_MEMORY_BYTES: &str = "process_virtual_memory_bytes";
/// The virtual memory limit, 0 when unlimited.
const VIRTUAL_MEMORY_MAX_BYTES: &str = "process_virtual_memory_max_bytes";
/// Resident memory size.
const RESIDENT_MEMORY_BYTES: &str = "process_resident_memory_bytes";
/// Process start time.
const START_TIME_SECONDS: &str = "process_start_time_seconds";
/// OS threads.
const THREADS: &str = "process_threads";

/// The kernel's clock ticks per second, the unit `/proc/self/stat` reports CPU time in.
static TICKS_PER_SECOND: LazyLock<f64> = LazyLock::new(|| procfs::ticks_per_second() as f64);

/// The system boot time, which process start times in `/proc` are relative to.
static BOOT_TIME_SECS: LazyLock<Option<u64>> = LazyLock::new(|| procfs::boot_time_secs().ok());

/// One reading of the standard Prometheus process metrics, each absent when `/proc` did not answer.
#[derive(Debug, Default, PartialEq)]
struct Snapshot {
    cpu_seconds_total: Option<f64>,
    open_fds: Option<u64>,
    max_fds: Option<u64>,
    virtual_memory_bytes: Option<u64>,
    virtual_memory_max_bytes: Option<u64>,
    resident_memory_bytes: Option<u64>,
    start_time_seconds: Option<u64>,
    threads: Option<u64>,
}

/// Reads this process's metrics from `/proc/self`.
fn snapshot() -> Snapshot {
    let mut snapshot = Snapshot::default();
    let Ok(process) = Process::myself() else {
        return snapshot;
    };
    if let Ok(stat) = process.stat() {
        snapshot.start_time_seconds =
            BOOT_TIME_SECS.map(|boot| boot + ((stat.starttime as f64) / *TICKS_PER_SECOND) as u64);
        snapshot.cpu_seconds_total = Some((stat.utime + stat.stime) as f64 / *TICKS_PER_SECOND);
        snapshot.resident_memory_bytes = Some(stat.rss_bytes().get());
        snapshot.virtual_memory_bytes = Some(stat.vsize);
        snapshot.threads = Some(stat.num_threads as u64);
    }
    snapshot.open_fds = process.fd_count().ok().map(|count| count as u64);
    if let Ok(limits) = process.limits() {
        snapshot.max_fds = Some(limit_or_zero(limits.max_open_files.soft_limit));
        snapshot.virtual_memory_max_bytes =
            Some(limit_or_zero(limits.max_address_space.soft_limit));
    }
    snapshot
}

/// A soft limit's value, with 0 standing for unlimited as Prometheus expects.
fn limit_or_zero(limit: LimitValue) -> u64 {
    match limit {
        LimitValue::Value(value) => value,
        LimitValue::Unlimited => 0,
    }
}

/// Registers the help text and unit of every process metric.
pub(crate) fn describe() {
    describe_counter!(
        CPU_SECONDS_TOTAL,
        Unit::Seconds,
        "Total user and system CPU time spent in seconds."
    );
    describe_gauge!(OPEN_FDS, Unit::Count, "Number of open file descriptors.");
    describe_gauge!(
        MAX_FDS,
        Unit::Count,
        "Maximum number of open file descriptors."
    );
    describe_gauge!(
        VIRTUAL_MEMORY_BYTES,
        Unit::Bytes,
        "Virtual memory size in bytes."
    );
    describe_gauge!(
        VIRTUAL_MEMORY_MAX_BYTES,
        Unit::Bytes,
        "Maximum amount of virtual memory available in bytes."
    );
    describe_gauge!(
        RESIDENT_MEMORY_BYTES,
        Unit::Bytes,
        "Resident memory size in bytes."
    );
    describe_gauge!(
        START_TIME_SECONDS,
        Unit::Seconds,
        "Start time of the process since unix epoch in seconds."
    );
    describe_gauge!(THREADS, Unit::Count, "Number of OS threads in the process.");
}

/// Records one reading of every process metric that `/proc` answered.
pub(crate) fn collect() {
    let snapshot = snapshot();
    if let Some(value) = snapshot.cpu_seconds_total {
        counter!(CPU_SECONDS_TOTAL).absolute(value as u64);
    }
    let gauges = [
        (OPEN_FDS, snapshot.open_fds),
        (MAX_FDS, snapshot.max_fds),
        (VIRTUAL_MEMORY_BYTES, snapshot.virtual_memory_bytes),
        (VIRTUAL_MEMORY_MAX_BYTES, snapshot.virtual_memory_max_bytes),
        (RESIDENT_MEMORY_BYTES, snapshot.resident_memory_bytes),
        (START_TIME_SECONDS, snapshot.start_time_seconds),
        (THREADS, snapshot.threads),
    ];
    for (name, value) in gauges {
        if let Some(value) = value {
            gauge!(name).set(value as f64);
        }
    }
}

#[cfg(test)]
mod snapshot {
    #[test]
    fn reads_every_metric_from_proc() {
        let snapshot = super::snapshot();

        assert!(snapshot.cpu_seconds_total.is_some());
        assert!(snapshot.open_fds.is_some_and(|count| count > 0));
        assert!(snapshot.max_fds.is_some());
        assert!(snapshot.virtual_memory_bytes.is_some_and(|bytes| bytes > 0));
        assert!(snapshot.virtual_memory_max_bytes.is_some());
        assert!(snapshot
            .resident_memory_bytes
            .is_some_and(|bytes| bytes > 0));
        assert!(snapshot
            .start_time_seconds
            .is_some_and(|seconds| seconds > 0));
        assert!(snapshot.threads.is_some_and(|threads| threads > 0));
    }
}
