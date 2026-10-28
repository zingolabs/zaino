//! `zainod overload on|off|set|unset`: operator markers in `<serve.state_dir>/overload/` over
//! `[grpc.overload]`
//!
//! - `off` = never `Overloaded`; `<key>` = that key's value; both survive restarts (as `held`)
//! - the running zainod reads them every second → the gRPC overload latch (a change = WARN; never
//!   on public `/status`)

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;
use zaino_grpc::{AdmissionState, OverloadLimits};

use super::{remove_marker, write_marker, MarkerError};

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
    fn unmarked(config: &OverloadLimits) -> Self {
        Self { limits: config.clone(), detection: true, overrides: Vec::new() }
    }

    /// Log notes (none while nothing is overridden)
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
///
/// - each marker checked as it lands: a refusal names its `key = value`
fn merged(config: &OverloadLimits, markers: &[(String, String)]) -> Result<OverloadLimits, String> {
    let mut table =
        toml::Table::try_from(config).expect("OverloadLimits serializes to a TOML table");
    let mut limits = config.clone();
    for (key, value) in markers {
        let refused = |why: &str| format!("{key} = {value}: {why}");
        let parsed = toml::from_str::<toml::Table>(&format!("value = {value}"))
            .ok()
            .and_then(|mut parsed| parsed.remove("value"))
            .ok_or_else(|| refused("not a value"))?;
        table.insert(key.clone(), parsed);
        limits = table.clone().try_into().map_err(|e: toml::de::Error| refused(e.message()))?;
    }
    Ok(limits)
}

/// `dir`'s markers over `config` (no `dir` = no markers)
fn read(dir: &Path, config: &OverloadLimits) -> Result<Settings, MarkerError> {
    let io = |path: &Path, source| MarkerError::Io { path: path.to_owned(), source };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::unmarked(config))
        }
        Err(error) => return Err(io(dir, error)),
    };
    let mut markers = Vec::new();
    let mut detection = true;
    for entry in entries {
        let entry = entry.map_err(|error| io(dir, error))?;
        let key = entry.file_name().to_string_lossy().into_owned();
        match key.as_str() {
            OFF => detection = false,
            _ => {
                let path = entry.path();
                let value = std::fs::read_to_string(&path).map_err(|error| io(&path, error))?;
                markers.push((key, value.trim().to_owned()));
            }
        }
    }
    markers.sort();
    let limits = merged(config, &markers).map_err(MarkerError::Invalid)?;
    let overrides = markers.iter().map(|(key, value)| format!("{key} = {value}")).collect();
    Ok(Settings { limits, detection, overrides })
}

/// The marker `action` names written or removed (`Set` validated first)
fn apply(dir: &Path, config: &OverloadLimits, action: &Action) -> Result<(), MarkerError> {
    match action {
        Action::Off => write_marker(dir, OFF, ""),
        Action::Set { key, value } => {
            merged(config, &[(key.clone(), value.clone())]).map_err(MarkerError::Invalid)?;
            write_marker(dir, key, value)
        }
        Action::On => remove_marker(dir, OFF).map(drop),
        Action::Unset { key } => remove_marker(dir, key).map(drop),
    }
}

/// `zainod overload …`: prints the settings now in force
pub(crate) fn command(config_path: &Path, action: Action) -> Result<ExitCode, MarkerError> {
    let config = crate::config::load_config(config_path)?;
    let dir = config.serve.state_dir.join("overload");
    let limits = &config.grpc.overload;
    apply(&dir, limits, &action)?;
    let settings = read(&dir, limits)?;
    let printed = toml::to_string(&settings.limits).expect("OverloadLimits serializes to TOML");
    println!("detection = {}", settings.detection);
    print!("{printed}");
    eprintln!(
        "overridden: [{}] in {} (a running zainod applies within 1 s)",
        settings.overrides.join(", "),
        dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Every second for the process's life: the markers → `admission` (a change = WARN; unreadable or
/// invalid = WARN, settings kept)
pub(crate) fn spawn(admission: Arc<AdmissionState>, state_dir: &Path, config: OverloadLimits) {
    let dir: PathBuf = state_dir.join("overload");
    tokio::spawn(async move {
        let mut current = Settings::unmarked(&config);
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
    /// - notes name what is in force, none once all is undone
    #[test]
    fn markers_override_the_config_until_undone() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("overload");
        let config = OverloadLimits::default();
        let run = |action: Action| apply(&dir, &config, &action);
        let set =
            |key: &str, value: &str| Action::Set { key: key.to_owned(), value: value.to_owned() };
        let defaults = Settings { limits: config.clone(), detection: true, overrides: vec![] };

        assert_eq!(read(&dir, &config).expect("reads"), defaults, "no markers: the config");
        for action in [
            Action::Off,
            Action::Off,
            set("io_pressure_percent", "10"),
            set("cpu_pressure_percent", "40"),
        ] {
            run(action).expect("applies");
        }
        let expected = Settings {
            limits: OverloadLimits { cpu_pressure_percent: 40, io_pressure_percent: 10 },
            detection: false,
            overrides: vec!["cpu_pressure_percent = 40".into(), "io_pressure_percent = 10".into()],
        };
        assert_eq!(read(&dir, &config).expect("reads"), expected, "off twice + two keys set");
        assert_eq!(
            expected.notes(),
            [
                "Overload detection off (zainod overload on)",
                "Overload overridden: cpu_pressure_percent = 40, io_pressure_percent = 10 \
                 (zainod overload unset)",
            ],
            "notes while overridden"
        );

        for action in [
            Action::On,
            Action::On,
            Action::Unset { key: "cpu_pressure_percent".into() },
            Action::Unset { key: "io_pressure_percent".into() },
        ] {
            run(action).expect("applies");
        }
        assert_eq!(read(&dir, &config).expect("reads"), defaults, "all undone");
        assert_eq!(defaults.notes(), Vec::<String>::new(), "nothing overridden: no notes");
    }

    /// Invalid key or value refused naming it, before anything is written
    #[test]
    fn a_bad_override_is_refused_naming_it_and_never_lands() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("overload");
        let config = OverloadLimits::default();

        #[rustfmt::skip]
        let refusals = [
            // key,                    value, refusal names
            ("nope",                   "1",   "nope = 1: unknown field `nope`"),
            ("cpu_pressure_percent",   "300", "cpu_pressure_percent = 300: invalid value"),
            ("io_pressure_percent",    "x",   "io_pressure_percent = x: not a value"),
        ];
        for (key, value, named) in refusals {
            let set = Action::Set { key: key.to_owned(), value: value.to_owned() };
            let refused = apply(&dir, &config, &set).map_err(|error| error.to_string());
            assert!(
                refused.as_ref().is_err_and(|refused| refused.contains(named)),
                "{key} = {value}: refused naming {named:?}, got {refused:?}"
            );
        }
        let untouched = Settings { limits: config.clone(), detection: true, overrides: vec![] };
        assert_eq!(read(&dir, &config).expect("reads"), untouched, "nothing refused landed");
    }

    /// Marker dir present but unlistable (a file here) = error naming it, never "no markers"
    #[test]
    fn an_unreadable_marker_dir_is_an_error_not_the_config() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("overload");
        std::fs::write(&dir, "").expect("file where the dir goes");

        let read = read(&dir, &OverloadLimits::default());
        assert!(
            matches!(&read, Err(MarkerError::Io { path, .. }) if *path == dir),
            "Io error naming {}, got {read:?}",
            dir.display()
        );
    }
}
