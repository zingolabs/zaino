use clap::Args as ClapArgs;

use crate::commands::changeset::ChangesetCommandError;
use crate::context::Ctx;

/// `relman changeset rename --pr <N>`.
///
/// The PR-gate bot step: rename this PR's author changeset(s) — the random
/// `adjective-noun` slug(s) — to the canonical `pr-<N>` name(s). Accumulated
/// `pr-*` files from earlier merged PRs are left untouched, and the command is
/// a no-op (exit 0) when the PR carries no author changesets, so the bot may
/// safely re-run it.
#[derive(ClapArgs)]
pub struct Args {
    /// The PR number to rename this PR's author changeset(s) under.
    #[arg(long, value_name = "N")]
    pr: u32,
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangesetCommandError> {
    let renamed = ctx.changesets.rename_to_pr(args.pr)?;
    if renamed.is_empty() {
        println!("relman: no author changesets to rename");
        return Ok(());
    }
    for slug in &renamed {
        println!("relman: renamed a changeset to {}", slug.file_name());
    }
    Ok(())
}
