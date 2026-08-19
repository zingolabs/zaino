use clap::Args as ClapArgs;

use relman_core::ports::ArtifactError;
use relman_core::types::{CycleId, InvalidCycleId};

use crate::context::Ctx;

/// `relman pr-body --cycle <ID>` — render the release-PR markdown to stdout.
#[derive(ClapArgs)]
pub struct Args {
    /// The release-cycle identifier (e.g. `2026-08-15`).
    #[arg(long)]
    cycle: String,
}

/// What can go wrong running `relman pr-body`.
#[derive(Debug, thiserror::Error)]
pub enum PrBodyCommandError {
    /// The `--cycle` value was not a valid cycle id.
    #[error("invalid --cycle value")]
    Cycle(#[from] InvalidCycleId),
    /// Rendering the PR body failed.
    #[error("could not render the release-PR body")]
    Artifact(#[from] ArtifactError),
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), PrBodyCommandError> {
    let cycle = CycleId::parse(&args.cycle)?;
    let body = ctx.release_artifacts.pr_body(&cycle)?;
    print!("{body}");
    Ok(())
}
