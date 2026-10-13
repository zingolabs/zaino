//! Zaino indexer daemon

use std::fmt::Display;
use std::process::ExitCode;

use clap::Parser;

use zainodlib::cli::{Cli, Command};

fn main() -> ExitCode {
    // a panic anywhere = a violated invariant: print it, then die (never unwound into a task
    // boundary that keeps serving)
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));

    let cli = Cli::parse();

    // logging → stdout by default (reserved for `verify`'s JSON and `hold`/`resume`'s state)
    if !matches!(cli.command, Command::Verify | Command::Hold(_)) {
        if let Err(error) = zainodlib::logging::init() {
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

    let config = cli.config.unwrap_or_else(zainodlib::paths::default_config);
    let code = runtime.block_on(async {
        match cli.command {
            Command::Start => exit(zainodlib::run(config).await.map(|()| ExitCode::SUCCESS), 1),
            Command::GenerateConfig { output } => {
                exit(zainodlib::generate_config(&output.unwrap_or(config)), 1)
            }
            Command::Verify => exit(zainodlib::verify::run(&config), 2),
            Command::Hold(action) => exit(zainodlib::hold::command(&config, action), 1),
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
