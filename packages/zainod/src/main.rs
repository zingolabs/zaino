//! Zaino indexer daemon

#![forbid(unsafe_code)]

use std::fmt::Display;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};

mod cli;
mod config;
mod error;

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
    let config = cli.config.unwrap_or_else(config::paths::default_config);
    match cli.command {
        Command::GenerateConfig { output } => {
            exit(config::generate_config(&output.unwrap_or(config)), 1)
        }
    }
}

/// `Err` → stderr + `failure`
fn exit(outcome: Result<ExitCode, impl Display>, failure: u8) -> ExitCode {
    outcome.unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        ExitCode::from(failure)
    })
}
