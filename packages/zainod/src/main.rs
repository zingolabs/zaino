//! Zaino indexer daemon

#![forbid(unsafe_code)]

use std::fmt::Display;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tracing::{error, info, Instrument as _};

use crate::cli::{Cli, Command};
use crate::command::{hold, overload, verify};
use crate::config::load_config;
use crate::error::IndexerError;
use crate::health::{admin, notify, state_log, DaemonStatus};
use crate::telemetry::logging;

mod cli;
mod command;
mod config;
mod daemon;
mod error;
mod health;
mod telemetry;

// Every build, not musl only (profiles + benchmarks measure the shipped allocator)
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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
            eprintln!("Error: RUST_LOG: {error}");
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

    let Some(config) = cli.config.or_else(config::paths::default_config) else {
        eprintln!("Error: no home directory to find the config file in: pass --config");
        return ExitCode::FAILURE;
    };
    let code = runtime.block_on(async {
        match cli.command {
            Command::Start => {
                let run = start(config).instrument(logging::component("Zainod"));
                exit(run.await.map(|()| ExitCode::SUCCESS), 1)
            }
            Command::GenerateConfig { output } => {
                exit(config::generate_config(&output.unwrap_or(config)), 1)
            }
            Command::Verify => verify::run(&config),
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
async fn start(config_path: PathBuf) -> Result<(), IndexerError> {
    info!(version = env!("CARGO_PKG_VERSION"), "Starting");
    let config = load_config(&config_path)?;
    let enabled = config.indexes.iter().map(|index| index.kind);
    let status = Arc::new(DaemonStatus::new(config.network, enabled));
    // Before the index-snapshot bootstrap (a held node reads held from its first answer)
    let (state_dir, goaway_delay) =
        (config.serve.state_dir.clone(), config.serve.hold_goaway_delay);
    hold::spawn(Arc::clone(&status), state_dir, goaway_delay);
    config.warn_about_metrics_listener();
    // Before the index-snapshot bootstrap (its progress extends systemd's start timeout)
    notify::spawn(Arc::clone(&status));
    state_log::spawn(Arc::clone(&status));

    if let Some(endpoint) = config.metrics.listen_address {
        let status = Arc::clone(&status);
        logging::component("Admin").in_scope(|| admin::start(endpoint, status))?;
    }
    // After the admin listener (`/readyz` reports the index-snapshot install inside)
    let running = daemon::start(config, status).await.inspect_err(|error| {
        error!(%error, "Startup failed");
    })?;
    match running.await.unwrap_or_else(|join| daemon::rejoin(join)) {
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
