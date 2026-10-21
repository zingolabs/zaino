//! `zainod overload on|off|set|unset`: operator markers in `<serve.state_dir>/overload/` over
//! `[grpc.overload]`
//!
//! - `off` = never `Overloaded`; `<key>` = that key's value; both survive restarts (as `held`)
//! - the running zainod reads them every second → the gRPC overload latch + `/status` notes

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;
use zaino_grpc::{AdmissionState, OverloadLimits};

use crate::status::DaemonStatus;

const OFF: &str = "off";

#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub(crate) enum Action {
    /// Report `Overloaded` again when a cause latches.
    On,
    /// Never report `Overloaded` (a misfiring detector keeps the node in rotation).
    Off,
    /// Override one `[grpc.overload]` key, e.g. `cpu_pressure_percent 40`.
    Set { key: String, value: String },
    /// Back to the config's value for one key.
    Unset { key: String },
}

/// The config with every marker applied
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    limits: OverloadLimits,
    detection: bool,
    overrides: Vec<String>,
}

impl Settings {
    /// `/status` notes (none while nothing is overridden)
    fn notes(&self) -> Vec<String> {
        let off =
            (!self.detection).then(|| "Overload detection off (zainod overload on)".to_owned());
        let set = (!self.overrides.is_empty()).then(|| {
            format!("Overload overridden: {} (zainod overload unset)", self.overrides.join(", "))
        });
        off.into_iter().chain(set).collect()
    }
}

/// `config` with `markers` (`key`, `value`) applied, by `[grpc.overload]`'s own rules
fn merged(config: &OverloadLimits, markers: &[(String, String)]) -> Result<OverloadLimits, String> {
    let mut table = toml::Table::try_from(config).map_err(|error| error.to_string())?;
    for (key, value) in markers {
        let value = toml::from_str::<toml::Table>(&format!("value = {value}"))
            .ok()
            .and_then(|mut parsed| parsed.remove("value"))
            .ok_or_else(|| format!("{key} = {value}: not a value"))?;
        table.insert(key.clone(), value);
    }
    table.try_into().map_err(|error: toml::de::Error| error.message().to_owned())
}

/// `dir`'s markers over `config`
fn read(dir: &Path, config: &OverloadLimits) -> Result<Settings, String> {
    let mut markers = Vec::new();
    let mut detection = true;
    for entry in std::fs::read_dir(dir).into_iter().flatten() {
        let entry = entry.map_err(|error| error.to_string())?;
        let key = entry.file_name().to_string_lossy().into_owned();
        match key.as_str() {
            OFF => detection = false,
            _ => {
                let value =
                    std::fs::read_to_string(entry.path()).map_err(|error| error.to_string())?;
                markers.push((key, value.trim().to_owned()));
            }
        }
    }
    markers.sort();
    let limits = merged(config, &markers)?;
    let overrides = markers.iter().map(|(key, value)| format!("{key} = {value}")).collect();
    Ok(Settings { limits, detection, overrides })
}

/// The marker `action` names written or removed (`Set` validated first), its directory synced: it
/// must survive a reboot right after the command
fn apply(dir: &Path, config: &OverloadLimits, action: &Action) -> Result<(), String> {
    match action {
        Action::Off => write(dir, OFF, "")?,
        Action::Set { key, value } => {
            merged(config, &[(key.clone(), value.clone())])?;
            write(dir, key, value)?;
        }
        Action::On => remove(dir, OFF)?,
        Action::Unset { key } => remove(dir, key)?,
    }
    absent_ok(dir, std::fs::File::open(dir).and_then(|dir| dir.sync_all()))
}

fn write(dir: &Path, name: &str, value: &str) -> Result<(), String> {
    let path = dir.join(name);
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&path, format!("{value}\n")))
        .and_then(|()| std::fs::File::open(&path)?.sync_all());
    written.map_err(|error| format!("{}: {error}", path.display()))
}

fn remove(dir: &Path, name: &str) -> Result<(), String> {
    let path = dir.join(name);
    absent_ok(&path, std::fs::remove_file(&path))
}

/// `result` at `path`, a missing file counting as done
fn absent_ok(path: &Path, result: std::io::Result<()>) -> Result<(), String> {
    match result {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {error}", path.display()))
        }
        _ => Ok(()),
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum OverloadError {
    #[error(transparent)]
    Config(#[from] crate::error::IndexerError),
    #[error("{0}")]
    Marker(String),
}

/// `zainod overload …`: prints the settings now in force
pub(super) fn command(config_path: &Path, action: Action) -> Result<ExitCode, OverloadError> {
    let config = crate::config::load_config(config_path)?;
    let dir = config.serve.state_dir.join("overload");
    let limits = &config.grpc.overload;
    apply(&dir, limits, &action).map_err(OverloadError::Marker)?;
    let settings = read(&dir, limits).map_err(OverloadError::Marker)?;
    println!("detection = {}", settings.detection);
    print!(
        "{}",
        toml::to_string(&settings.limits)
            .map_err(|error| OverloadError::Marker(error.to_string()))?
    );
    eprintln!(
        "overridden: [{}] in {} (a running zainod applies within 1 s)",
        settings.overrides.join(", "),
        dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Every second for the process's life: the markers → `admission` + `status` notes (a change =
/// WARN; unreadable or invalid = WARN, settings kept)
pub(crate) fn spawn(
    admission: Arc<AdmissionState>,
    status: Arc<DaemonStatus>,
    state_dir: &Path,
    config: OverloadLimits,
) {
    let dir: PathBuf = state_dir.join("overload");
    tokio::spawn(async move {
        let mut current =
            Settings { limits: config.clone(), detection: true, overrides: Vec::new() };
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let settings = match read(&dir, &config) {
                Ok(settings) if settings == current => continue,
                Ok(settings) => settings,
                Err(error) => {
                    warn!(%error, "Overload markers ignored, settings kept");
                    continue;
                }
            };
            admission.set_overload(settings.limits.clone(), settings.detection);
            status.set_overload_notes(settings.notes());
            warn!(notes = ?settings.notes(), "Overload settings changed by operator");
            current = settings;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session of commands, read back as the running zainod reads it:
    /// - off / on toggle detection; set / unset one key over the config's (twice = once)
    /// - an invalid key or value is refused before anything is written
    /// - notes name what is in force, none once all is undone
    #[test]
    fn markers_override_the_config_until_undone_and_bad_values_never_land() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("overload");
        let config = OverloadLimits::default();
        let run = |action: Action| apply(&dir, &config, &action);
        let set =
            |key: &str, value: &str| Action::Set { key: key.to_owned(), value: value.to_owned() };
        let defaults = Settings { limits: config.clone(), detection: true, overrides: vec![] };

        assert_eq!(read(&dir, &config), Ok(defaults.clone()), "no markers: the config");
        for action in [
            Action::Off,
            Action::Off,
            set("network_queue_delay_ms", "10"),
            set("cpu_pressure_percent", "40"),
        ] {
            run(action).expect("applies");
        }
        let expected = Settings {
            limits: OverloadLimits {
                network_queue_delay_ms: 10,
                cpu_pressure_percent: 40,
                ..config.clone()
            },
            detection: false,
            overrides: vec![
                "cpu_pressure_percent = 40".into(),
                "network_queue_delay_ms = 10".into(),
            ],
        };
        assert_eq!(read(&dir, &config), Ok(expected.clone()));
        assert_eq!(
            expected.notes(),
            [
                "Overload detection off (zainod overload on)",
                "Overload overridden: cpu_pressure_percent = 40, network_queue_delay_ms = 10 \
                 (zainod overload unset)",
            ]
        );

        for refused in
            [set("nope", "1"), set("network_min_networks", "0"), set("io_pressure_percent", "x")]
        {
            assert!(run(refused.clone()).is_err(), "{refused:?}");
        }
        assert_eq!(read(&dir, &config), Ok(expected), "nothing refused landed");

        for action in [
            Action::On,
            Action::On,
            Action::Unset { key: "cpu_pressure_percent".into() },
            Action::Unset { key: "network_queue_delay_ms".into() },
        ] {
            run(action).expect("applies");
        }
        assert_eq!(read(&dir, &config), Ok(defaults.clone()), "all undone");
        assert!(defaults.notes().is_empty());
    }
}
