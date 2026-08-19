use clap::Args as ClapArgs;

use relman_core::ports::ArtifactError;
use relman_core::types::{CycleId, InvalidCycleId};

use crate::context::Ctx;
use crate::format;

/// `relman tags --cycle <ID> [--rc <N>]` — print the tag plan for a cycle, one
/// tag per line, for CI to `git tag` verbatim.
#[derive(ClapArgs)]
pub struct Args {
    /// The release-cycle identifier (e.g. `2026-08-15`).
    #[arg(long)]
    cycle: String,
    /// The soak/prerelease number: with it, emit a single `cycle-<id>-rc.<N>`
    /// tag; without it, emit the blessing tag set (cycle + per-crate versions).
    #[arg(long)]
    rc: Option<u32>,
}

/// What can go wrong running `relman tags`.
#[derive(Debug, thiserror::Error)]
pub enum TagsCommandError {
    /// The `--cycle` value was not a valid cycle id.
    #[error("invalid --cycle value")]
    Cycle(#[from] InvalidCycleId),
    /// Computing the tag plan failed.
    #[error("could not compute the tag plan")]
    Artifact(#[from] ArtifactError),
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), TagsCommandError> {
    let cycle = CycleId::parse(&args.cycle)?;
    crate::warn::unfilled_templates(ctx);
    let plan = ctx.release_artifacts.tags(&cycle, args.rc)?;
    print!("{}", format::tag_plan(&plan));
    Ok(())
}
