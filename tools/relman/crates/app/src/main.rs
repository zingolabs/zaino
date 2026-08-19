//! Composition root.
//!
//! The only place that names concrete adapters. It constructs adapters, wires
//! them into services, and dispatches CLI commands. Everything else depends on
//! ports, not implementations — so swapping an adapter (e.g. a fixed clock for
//! the system one) touches only this file.
//!
//! Slice 0 wires a single trivial live thread — `relman about` — end to end
//! through the hexagon: CLI → `About` driving port → `AboutService` → `Clock`
//! driven port. Later slices add the real release adapters and commands.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use relman_cli::{Cli, Command, Ctx, commands};
use relman_core::ports::Clock;
use relman_core::types::{DateTime, Utc};
use relman_domain::services::AboutService;

/// Real system-clock adapter for the [`Clock`] driven port.
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::About(args) => with_ctx(|ctx| {
            commands::about::run(args, ctx);
            Ok(())
        }),
    }
}

/// Build the driving-port context and hand it to a command.
fn with_ctx(f: impl FnOnce(&Ctx) -> Result<()>) -> Result<()> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let about = Arc::new(AboutService::new(clock));

    let ctx = Ctx { about };
    f(&ctx)
}
