//! CPU / IO pressure (Linux PSI): share of a window this process's cgroup waited, else the
//! system's (`/proc/pressure`), else none
//!
//! - share = Δ `some total=` µs / window (exact, not the kernel's 10 s / 60 s averages)

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
        let own = cgroup.map(|dir| dir.join(format!("{resource}.pressure")));
        let system = Some(PathBuf::from("/proc/pressure").join(resource));
        let file = [own, system].into_iter().flatten().find(|file| file.exists());
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
        Some((stalled as f64 / elapsed.as_micros().max(1) as f64).min(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `some total` parsed (`full` ignored); a window = Δ total / its length, capped at 1
    #[test]
    fn a_window_is_the_stalled_share_of_its_length() {
        let psi = |total: u64| format!("some avg10=1.00 total={total}\nfull avg10=0.00 total=9\n");
        assert_eq!(some_total_us(&psi(42)), Some(42));
        assert_eq!(some_total_us("full avg10=0.00 total=1\n"), None);

        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("cpu.pressure");
        let mut cpu = Pressure { file: Some(file.clone()), last_us: Some(1_000_000) };
        std::fs::write(&file, psi(16_000_000)).expect("write");
        assert_eq!(cpu.share(Duration::from_secs(60)), Some(0.25), "15 s of 60 s");
        std::fs::write(&file, psi(136_000_000)).expect("write");
        assert_eq!(cpu.share(Duration::from_secs(60)), Some(1.0), "capped");
        assert_eq!(Pressure { file: None, last_us: None }.share(Duration::from_secs(60)), None);
    }
}
