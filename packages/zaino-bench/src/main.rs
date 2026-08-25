//! Benchmark harness for Zaino.
//!
//! Answers three operational questions against a running zainod, from the
//! outside, over the interfaces a real client uses:
//!
//! - `sync`       — how long does an initial sync take?
//! - `concurrent` — how many concurrent connections can it support?
//! - `serve`      — how fast can it serve blocks on one stream?
//!
//! The measured configuration belongs with the numbers: see `docs/perf.md` for
//! results and `docs/example_configs/zainod-bench-mainnet.toml` for the node
//! config they were produced with.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod chain;
mod concurrent;
mod error;
mod grpc_client;
mod metrics;
mod serve;
mod stats;
mod sync;

#[derive(Parser)]
#[command(name = "zaino-bench", about, version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure how long zainod takes to sync to the chain tip.
    Sync(sync::SyncArgs),
    /// Load-test a running server with concurrent block-range clients.
    Concurrent(concurrent::ConcurrentArgs),
    /// Measure single-stream block serve rate, verifying the chain as it goes.
    Serve(serve::ServeArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    // Must run before any rustls config is built — the workspace enables
    // reqwest's `rustls-no-provider`, which never auto-selects one, so the
    // metrics scrape panics with "No provider set" without this (ADR-0006).
    zaino_common::crypto::ensure_default_crypto_provider();

    let result = match Cli::parse().command {
        Command::Sync(args) => sync::run(args).await,
        Command::Concurrent(args) => concurrent::run(args).await,
        Command::Serve(args) => serve::run(args).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!();
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_is_well_formed() {
        Cli::command().debug_assert();
    }
}
