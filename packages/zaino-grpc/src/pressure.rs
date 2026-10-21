//! CPU / IO pressure (Linux PSI) of this process's cgroup → the overload latch (`overload.rs`)
//!
//! - cgroup v2 `cpu.pressure` / `io.pressure` (zainod's own: a noisy neighbour on a shared host
//!   is not zainod's load), else system-wide `/proc/pressure/*`, else none (non-Linux, PSI off)
//! - share of a window = Δ `some total=` µs / window (exact, not the kernel's 10 s / 60 s averages)

use std::path::{Path, PathBuf};
use std::time::Duration;

/// `some … total=<µs>` of a PSI file (`None` = no `some` line / no `total`)
pub(crate) fn some_total_us(psi: &str) -> Option<u64> {
    let line = psi.lines().find(|line| line.starts_with("some "))?;
    line.split_whitespace().find_map(|field| field.strip_prefix("total="))?.parse().ok()
}

/// cgroup v2 path of this process from `/proc/self/cgroup` (`0::/system.slice/zainod.service`)
pub(crate) fn cgroup_dir(proc_self_cgroup: &str) -> Option<PathBuf> {
    let path = proc_self_cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// One resource's PSI file + its total at the last window
#[derive(Debug)]
struct Resource {
    file: Option<PathBuf>,
    last_us: Option<u64>,
}

impl Resource {
    fn new(cgroup: Option<&Path>, name: &str) -> Self {
        let candidates = [
            cgroup.map(|dir| dir.join(format!("{name}.pressure"))),
            Some(Path::new("/proc/pressure").join(name)),
        ];
        let file =
            candidates.into_iter().flatten().find(|file| std::fs::read_to_string(file).is_ok());
        let last_us = file.as_deref().and_then(read_total);
        Self { file, last_us }
    }

    /// Stalled share since the last call (`elapsed` long)
    fn share(&mut self, elapsed: Duration) -> Option<f64> {
        let now = read_total(self.file.as_deref()?)?;
        let stalled = now.checked_sub(self.last_us.replace(now)?)?;
        Some((stalled as f64 / elapsed.as_micros().max(1) as f64).min(1.0))
    }
}

fn read_total(file: &Path) -> Option<u64> {
    some_total_us(&std::fs::read_to_string(file).ok()?)
}

/// This process's CPU + IO pressure, read once per serving summary
#[derive(Debug)]
pub(crate) struct Pressure {
    cpu: Resource,
    io: Resource,
}

/// A window's stalled shares (`None` = unavailable here)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PressureShares {
    pub(crate) cpu: Option<f64>,
    pub(crate) io: Option<f64>,
}

impl Pressure {
    pub(crate) fn new() -> Self {
        let cgroup =
            std::fs::read_to_string("/proc/self/cgroup").ok().and_then(|text| cgroup_dir(&text));
        Self {
            cpu: Resource::new(cgroup.as_deref(), "cpu"),
            io: Resource::new(cgroup.as_deref(), "io"),
        }
    }

    pub(crate) fn window(&mut self, elapsed: Duration) -> PressureShares {
        PressureShares { cpu: self.cpu.share(elapsed), io: self.io.share(elapsed) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PSI text → `some` total (`full` ignored); cgroup v2 line → its pressure directory
    #[test]
    fn psi_some_total_and_the_cgroup_directory_parse_from_kernel_text() {
        let psi = "some avg10=12.50 avg60=3.10 avg300=0.80 total=987654321\n\
                   full avg10=0.00 avg60=0.00 avg300=0.00 total=42\n";
        assert_eq!(some_total_us(psi), Some(987_654_321));
        assert_eq!(some_total_us("full avg10=0.00 total=1\n"), None, "no some line");
        assert_eq!(some_total_us("some avg10=0.00\n"), None, "no total");

        let v2 = "0::/system.slice/zainod.service\n";
        assert_eq!(
            cgroup_dir(v2),
            Some(PathBuf::from("/sys/fs/cgroup/system.slice/zainod.service"))
        );
        assert_eq!(cgroup_dir("0::/\n"), Some(PathBuf::from("/sys/fs/cgroup")));
        assert_eq!(cgroup_dir("12:cpu,cpuacct:/user.slice\n"), None, "cgroup v1 only");
    }

    /// Share = Δ total / window, capped at 1; a missing file = no share
    #[test]
    fn a_window_share_is_the_stalled_microseconds_over_its_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("cpu.pressure");
        let write = |total: u64| {
            std::fs::write(&file, format!("some avg10=0.00 avg60=0.00 avg300=0.00 total={total}\n"))
                .expect("write psi")
        };
        write(1_000_000);
        let mut cpu = Resource { file: Some(file.clone()), last_us: read_total(&file) };
        write(16_000_000);
        assert_eq!(cpu.share(Duration::from_secs(60)), Some(0.25), "15 s stalled of 60 s");
        write(136_000_000);
        assert_eq!(cpu.share(Duration::from_secs(60)), Some(1.0), "capped");

        let mut missing = Resource { file: None, last_us: None };
        assert_eq!(missing.share(Duration::from_secs(60)), None);
    }
}
