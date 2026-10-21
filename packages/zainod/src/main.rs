//! Zaino indexer daemon

#![forbid(unsafe_code)]

use std::fmt::Display;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tracing::{error, info, Instrument as _};

use crate::cli::{Cli, Command};
use crate::config::load_config;
use crate::error::IndexerError;
use crate::status::DaemonStatus;

mod admin;
mod boot;
mod chainview;
mod cli;
mod config;
mod disk_monitor;
mod egress;
mod error;
mod fd_limit;
mod hold;
mod index_snapshot;
mod logging;
mod metrics;
mod notify;
mod overload;
mod paths;
mod peers;
mod progress;
mod schemas;
mod status;
mod utxos;
mod verify;

fn main() -> ExitCode {
    // a panic anywhere = a violated invariant: print it, then die (never unwound into a task
    // boundary that keeps serving)
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));

    let cli = Cli::parse();

    // logging → stdout by default (reserved for `verify`'s JSON, `hold`/`resume`'s and
    // `overload`'s state)
    if !matches!(cli.command, Command::Verify | Command::Hold(_) | Command::Overload { .. }) {
        if let Err(error) = logging::init() {
            eprintln!("Error: logging: {error}");
            return ExitCode::from(2);
        }
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Error: tokio runtime: {error}");
            return ExitCode::from(2);
        }
    };

    let config = cli.config.unwrap_or_else(paths::default_config);
    let code = runtime.block_on(async {
        match cli.command {
            Command::Start => exit(run(config).await.map(|()| ExitCode::SUCCESS), 1),
            Command::GenerateConfig { output } => {
                exit(config::generate_config(&output.unwrap_or(config)), 1)
            }
            Command::Verify => exit(verify::run(&config), 2),
            Command::Hold(action) => exit(hold::command(&config, action), 1),
            Command::Overload { action } => exit(overload::command(&config, action), 1),
        }
    });
    // never waits on blocking work a failed start left behind (it may never finish)
    runtime.shutdown_background();
    code
}

/// `Err` → stderr + `failure`
fn exit(outcome: Result<ExitCode, impl Display>, failure: u8) -> ExitCode {
    outcome.unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        ExitCode::from(failure)
    })
}

/// Until a shutdown signal (`Ok`) or the first failure (`Err`)
///
/// - no in-process restart: failure ends the process, the service manager restarts it, next boot
///   proves its state from disk (`zaino-persistence/usage.md` "The commit and crash contract")
async fn run(config_path: PathBuf) -> Result<(), IndexerError> {
    daemon(config_path).instrument(logging::component("Zainod")).await
}

async fn daemon(config_path: PathBuf) -> Result<(), IndexerError> {
    info!(version = env!("CARGO_PKG_VERSION"), "Starting");
    let config = load_config(&config_path)?;
    // Before any startup work (a bad `[snapshot]` must fail before it downloads)
    config.validate()?;
    let enabled = zaino_persistence::IndexKind::ALL.into_iter();
    let enabled = enabled.filter(|kind| config.enabled(*kind).is_some());
    let status = Arc::new(DaemonStatus::new(config.network, enabled));
    // Before the index-snapshot bootstrap (a held node reads held from its first answer)
    let (state_dir, goaway_delay) =
        (config.serve.state_dir.clone(), config.serve.hold_goaway_delay);
    hold::spawn(Arc::clone(&status), state_dir, goaway_delay);
    config.warn_about_metrics_listener();
    // Before the index-snapshot bootstrap (its progress extends systemd's start timeout)
    notify::spawn(Arc::clone(&status));

    if let Some(endpoint) = config.metrics.listen_address {
        let status = Arc::clone(&status);
        logging::component("Admin").in_scope(|| admin::start(endpoint, status))?;
    }
    // After the admin listener (`/readyz` reports the index-snapshot install inside)
    let running = boot::start(config, status).await.inspect_err(|error| {
        error!(%error, "Startup failed");
    })?;
    match running.await? {
        Ok(()) => {
            info!("Shutdown complete");
            Ok(())
        }
        Err(error) => {
            error!(%error, "Stopped with error");
            Err(error)
        }
    }
}
