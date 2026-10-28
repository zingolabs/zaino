//! CPU / IO pressure (Linux PSI): share of a window this process's cgroup waited, else unmeasured
//!
//! - share = Δ `some total=` µs / window (exact, not the kernel's 10 s / 60 s averages)
//! - never `/proc/pressure` (other processes' stalls would latch zainod's overload)

use std::path::PathBuf;
use std::time::Duration;

/// `some … total=<µs>` of a PSI file
fn some_total_us(psi: &str) -> Option<u64> {
    let some = psi.lines().find(|line| line.starts_with("some "))?;
    some.split_whitespace().find_map(|field| field.strip_prefix("total="))?.parse().ok()
}

/// One resource's PSI file + its total at the last window
#[derive(Debug)]
pub(crate) struct Pressure {
    file: Option<PathBuf>,
    last_us: Option<u64>,
}

impl Pressure {
    /// `resource` = `cpu` | `io`
    pub(crate) fn new(resource: &str) -> Self {
        // cgroup v2: `0::/system.slice/zainod.service`
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok().and_then(|text| {
            let path = text.lines().find_map(|line| line.strip_prefix("0::"))?.to_owned();
            Some(PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
        });
        let file =
            cgroup.map(|dir| dir.join(format!("{resource}.pressure"))).filter(|file| file.exists());
        let mut pressure = Self { file, last_us: None };
        pressure.last_us = pressure.read();
        pressure
    }

    fn read(&self) -> Option<u64> {
        some_total_us(&std::fs::read_to_string(self.file.as_ref()?).ok()?)
    }

    /// Stalled share since the last call (`elapsed` long)
    pub(crate) fn share(&mut self, elapsed: Duration) -> Option<f64> {
        let now = self.read()?;
        let stalled = now.checked_sub(self.last_us.replace(now)?)?;
        let window = elapsed.max(Duration::from_micros(1));
        Some(Duration::from_micros(stalled).div_duration_f64(window).min(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psi(some_total_us: u64) -> String {
        format!("some avg10=1.00 total={some_total_us}\nfull avg10=0.00 total=9\n")
    }

    /// Window = Δ `some` total (never `full`'s) / its length, capped at 1; no PSI file or no
    /// `some` line = unmeasured
    #[test]
    fn a_window_is_the_stalled_share_of_its_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("cpu.pressure");
        let mut cpu = Pressure { file: Some(file.clone()), last_us: Some(1_000_000) };
        let minute = Duration::from_secs(60);

        std::fs::write(&file, psi(16_000_000)).expect("write");
        let quarter = cpu.share(minute);
        std::fs::write(&file, psi(136_000_000)).expect("write");
        let over = cpu.share(minute);
        std::fs::write(&file, "full avg10=0.00 total=1\n").expect("write");
        let no_some_line = cpu.share(minute);

        assert_eq!(quarter, Some(0.25), "15 s stalled of 60 s");
        assert_eq!(over, Some(1.0), "120 s stalled of 60 s: capped");
        assert_eq!(no_some_line, None, "no `some` line");
        assert_eq!(Pressure { file: None, last_us: None }.share(minute), None, "no PSI file");
    }
}
