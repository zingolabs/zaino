//! Operator overload overrides: `zainod overload on|off|set|unset`
//!
//! - markers in `<serve.state_dir>/overload/`: `off` = `Overloaded` never reported; `<param>` =
//!   that `[grpc.overload]` threshold, the value inside
//! - config = the baseline, a marker wins; markers survive restart, deploy, reboot (as `held`)
//! - the running zainod reads them every [`POLL`] → zaino-grpc's latch + `/status` notes
//! - no write endpoint: as `hold`, the state dir is the authority

use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;
use zaino_grpc::{AdmissionState, OverloadLimits};

use crate::status::DaemonStatus;

/// Marker check cadence (a command lands within this)
const POLL: Duration = Duration::from_secs(1);

const DIR: &str = "overload";
const OFF: &str = "off";

#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub(crate) enum Action {
    /// Report `Overloaded` again when a cause latches (undo `zainod overload off`).
    On,
    /// Never report `Overloaded`, across restarts, until `zainod overload on`.
    ///
    /// For a detector misfiring during an incident: the node stays in rotation. The signals are
    /// still measured and exported, and `/status` notes say detection is off.
    Off,
    /// Override one `[grpc.overload]` threshold, across restarts, until `zainod overload unset`.
    Set { param: Param, value: String },
    /// Back to the config's value for one threshold.
    Unset { param: Param },
}

/// A `[grpc.overload]` threshold (its marker file's name = the config key)
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
pub(crate) enum Param {
    EgressQueueDelayMs,
    EgressMinNetworks,
    RetransmitPercent,
    CpuPressurePercent,
    IoPressurePercent,
}

impl Param {
    const ALL: [Self; 5] = [
        Self::EgressQueueDelayMs,
        Self::EgressMinNetworks,
        Self::RetransmitPercent,
        Self::CpuPressurePercent,
        Self::IoPressurePercent,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::EgressQueueDelayMs => "egress_queue_delay_ms",
            Self::EgressMinNetworks => "egress_min_networks",
            Self::RetransmitPercent => "retransmit_percent",
            Self::CpuPressurePercent => "cpu_pressure_percent",
            Self::IoPressurePercent => "io_pressure_percent",
        }
    }

    /// `value` into `limits`, by the config's own rules (non-zero; a percent 0–100, 0 = off)
    fn apply(self, limits: &mut OverloadLimits, value: &str) -> Result<(), String> {
        let value = value.trim();
        let invalid = |rule: &str| format!("{} = {value:?}: {rule}", self.name());
        let percent = || match value.parse::<u8>() {
            Ok(percent) if percent <= 100 => Ok(percent),
            _ => Err(invalid("a whole percent, 0 (off) to 100")),
        };
        match self {
            Self::EgressQueueDelayMs => {
                limits.egress_queue_delay_ms =
                    value.parse::<NonZeroU32>().map_err(|_| invalid("milliseconds, at least 1"))?;
            }
            Self::EgressMinNetworks => {
                limits.egress_min_networks =
                    value.parse::<NonZeroUsize>().map_err(|_| invalid("networks, at least 1"))?;
            }
            Self::RetransmitPercent => limits.retransmit_percent = percent()?,
            Self::CpuPressurePercent => limits.cpu_pressure_percent = percent()?,
            Self::IoPressurePercent => limits.io_pressure_percent = percent()?,
        }
        Ok(())
    }

    fn get(self, limits: &OverloadLimits) -> String {
        match self {
            Self::EgressQueueDelayMs => limits.egress_queue_delay_ms.to_string(),
            Self::EgressMinNetworks => limits.egress_min_networks.to_string(),
            Self::RetransmitPercent => limits.retransmit_percent.to_string(),
            Self::CpuPressurePercent => limits.cpu_pressure_percent.to_string(),
            Self::IoPressurePercent => limits.io_pressure_percent.to_string(),
        }
    }
}

/// What the latch runs on: the config with every marker applied
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    limits: OverloadLimits,
    detection: bool,
    overridden: Vec<Param>,
}

impl Settings {
    fn of(config: &OverloadLimits) -> Self {
        Self { limits: config.clone(), detection: true, overridden: Vec::new() }
    }

    /// `/status` notes: one per kind of override in force (none = no notes)
    pub(crate) fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.detection {
            notes.push("Overload detection off (zainod overload on)".to_owned());
        }
        if !self.overridden.is_empty() {
            let set: Vec<String> = self
                .overridden
                .iter()
                .map(|param| format!("{} = {}", param.name(), param.get(&self.limits)))
                .collect();
            notes.push(format!(
                "Overload thresholds overridden: {} (zainod overload unset)",
                set.join(", ")
            ));
        }
        notes
    }
}

/// `dir`'s markers over `config` + each marker that would not parse (its threshold left at the
/// config's)
fn read(dir: &Path, config: &OverloadLimits) -> io::Result<(Settings, Vec<String>)> {
    let mut settings = Settings::of(config);
    settings.detection = !dir.join(OFF).try_exists()?;
    let mut invalid = Vec::new();
    for param in Param::ALL {
        let value = match std::fs::read_to_string(dir.join(param.name())) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match param.apply(&mut settings.limits, &value) {
            Ok(()) => settings.overridden.push(param),
            Err(problem) => invalid.push(problem),
        }
    }
    Ok((settings, invalid))
}

/// The marker `action` names written (synced, with its directory) or removed (idempotent)
///
/// - `Set` validated before anything is written
fn apply(dir: &Path, action: &Action) -> Result<(), OverloadError> {
    let marker_failed = |marker: PathBuf| move |source| OverloadError::Marker { marker, source };
    let write = |name: &str, value: &str| {
        let marker = dir.join(name);
        std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&marker, value))
            .and_then(|()| std::fs::File::open(&marker)?.sync_all())
            .map_err(marker_failed(marker))
    };
    let remove = |name: &str| {
        let marker = dir.join(name);
        match std::fs::remove_file(&marker) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(marker_failed(marker)(error))
            }
            _ => Ok(()),
        }
    };
    match action {
        Action::On => remove(OFF)?,
        Action::Off => write(OFF, "")?,
        Action::Set { param, value } => {
            param.apply(&mut OverloadLimits::default(), value).map_err(OverloadError::Value)?;
            write(param.name(), &format!("{}\n", value.trim()))?;
        }
        Action::Unset { param } => remove(param.name())?,
    }
    // the marker's own entry durable: an override must survive a reboot right after the command
    match std::fs::File::open(dir).and_then(|dir| dir.sync_all()) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(marker_failed(dir.to_owned())(error))
        }
        _ => Ok(()),
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum OverloadError {
    #[error(transparent)]
    Config(#[from] crate::error::IndexerError),
    #[error("{}: {source}", marker.display())]
    Marker { marker: PathBuf, source: io::Error },
    #[error("{0}")]
    Value(String),
}

/// `zainod overload …`: prints the settings now in force (stdout), what changed (stderr)
pub(super) fn command(config_path: &Path, action: Action) -> Result<ExitCode, OverloadError> {
    let config = crate::config::load_config(config_path)?;
    let dir = config.serve.state_dir.join(DIR);
    apply(&dir, &action)?;
    let (settings, invalid) = read(&dir, &config.grpc.overload)
        .map_err(|source| OverloadError::Marker { marker: dir.clone(), source })?;
    println!("detection {}", if settings.detection { "on" } else { "off" });
    for param in Param::ALL {
        let source = if settings.overridden.contains(&param) { "override" } else { "config" };
        println!("{} = {} ({source})", param.name(), param.get(&settings.limits));
    }
    for problem in invalid {
        eprintln!("ignored marker: {problem}");
    }
    eprintln!("markers in {}: a running zainod applies them within 1 s", dir.display());
    Ok(ExitCode::SUCCESS)
}

/// For the process's life: the markers every [`POLL`] → `admission`'s latch + `status`'s notes
///
/// - a change = WARN with what is in force (at boot too, when markers were left set)
/// - unreadable markers = last settings kept, warned
pub(crate) fn spawn(
    admission: Arc<AdmissionState>,
    status: Arc<DaemonStatus>,
    state_dir: &Path,
    config: OverloadLimits,
) {
    let dir = state_dir.join(DIR);
    tokio::spawn(async move {
        let mut current = Settings::of(&config);
        let mut tick = tokio::time::interval(POLL);
        loop {
            tick.tick().await;
            let (settings, invalid) = match read(&dir, &config) {
                Ok(read) => read,
                Err(error) => {
                    warn!(%error, dir = %dir.display(), "Overload markers unreadable, settings kept");
                    continue;
                }
            };
            if settings == current {
                continue;
            }
            for problem in invalid {
                warn!(problem, "Overload marker ignored");
            }
            admission.set_overload(settings.limits.clone(), settings.detection);
            status.set_overload_notes(settings.notes());
            warn!(
                detection = settings.detection,
                overrides = ?settings.notes(),
                "Overload settings changed by operator"
            );
            current = settings;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session of commands against a state dir, read back as the running zainod reads it:
    /// - off / on toggle `detection`, set / unset one threshold over the config's, twice = once
    /// - notes name every override in force, none once all are undone
    /// - an invalid `set` writes nothing; a hand-written marker that will not parse is reported,
    ///   its threshold left at the config's
    #[test]
    fn markers_override_the_config_until_undone_and_bad_values_never_apply() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("state").join(DIR);
        let config = OverloadLimits { cpu_pressure_percent: 30, ..OverloadLimits::default() };
        let now = || read(&dir, &config).expect("readable");
        let set = |param, value: &str| Action::Set { param, value: value.to_owned() };

        assert_eq!(now(), (Settings::of(&config), vec![]), "no markers: the config, detection on");

        for action in [
            Action::Off,
            Action::Off,
            set(Param::CpuPressurePercent, "60"),
            set(Param::EgressMinNetworks, " 3\n"),
        ] {
            apply(&dir, &action).expect("applies");
        }
        let expected = Settings {
            limits: OverloadLimits {
                cpu_pressure_percent: 60,
                egress_min_networks: NonZeroUsize::new(3).expect("3 is non-zero"),
                ..config.clone()
            },
            detection: false,
            overridden: vec![Param::EgressMinNetworks, Param::CpuPressurePercent],
        };
        assert_eq!(now(), (expected.clone(), vec![]));
        assert_eq!(
            expected.notes(),
            [
                "Overload detection off (zainod overload on)",
                "Overload thresholds overridden: egress_min_networks = 3, cpu_pressure_percent = 60 \
                 (zainod overload unset)",
            ]
        );

        for (param, value) in [(Param::CpuPressurePercent, "101"), (Param::EgressQueueDelayMs, "0")]
        {
            let refused = apply(&dir, &set(param, value)).expect_err("out of range");
            assert!(matches!(refused, OverloadError::Value(_)), "{refused}");
        }
        assert_eq!(now().0, expected, "a refused set changes nothing");

        std::fs::write(dir.join("io_pressure_percent"), "lots").expect("hand-written marker");
        let (settings, invalid) = now();
        assert_eq!(settings.limits.io_pressure_percent, config.io_pressure_percent);
        assert_eq!(invalid, ["io_pressure_percent = \"lots\": a whole percent, 0 (off) to 100"]);

        for action in [
            Action::On,
            Action::On,
            Action::Unset { param: Param::CpuPressurePercent },
            Action::Unset { param: Param::EgressMinNetworks },
            Action::Unset { param: Param::IoPressurePercent },
            Action::Unset { param: Param::IoPressurePercent },
        ] {
            apply(&dir, &action).expect("applies");
        }
        assert_eq!(now(), (Settings::of(&config), vec![]), "all undone: the config again");
        assert!(Settings::of(&config).notes().is_empty());
    }
}
