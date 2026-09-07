use clap::Args as ClapArgs;

use relman_core::ports::ArtifactError;

use crate::context::Ctx;
use crate::format;

/// `relman publish-plan` — print the bumping crates in publish (dependency)
/// order, one `crate version` per line.
#[derive(ClapArgs)]
pub struct Args {}

/// What can go wrong running `relman publish-plan`.
#[derive(Debug, thiserror::Error)]
pub enum PublishPlanCommandError {
    /// Computing the publish order failed.
    #[error("could not compute the publish plan")]
    Artifact(#[from] ArtifactError),
}

pub fn run(_args: &Args, ctx: &Ctx) -> Result<(), PublishPlanCommandError> {
    crate::warn::unfilled_templates(ctx);
    let plan = ctx.release_artifacts.publish_plan()?;
    // The note goes to stderr: stdout is the machine-read plan, one crate per
    // line, and an empty plan must leave it empty.
    if plan.is_empty() {
        eprintln!("relman: nothing to publish (no changesets affect a governed target)");
    }
    print!("{}", format::publish_plan(&plan));
    Ok(())
}
