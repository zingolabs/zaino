use clap::Args as ClapArgs;

use crate::commands::changeset::ChangesetCommandError;
use crate::context::Ctx;

/// `relman changeset clear [--yes]`.
///
/// A manual garbage-collect: remove every changeset file — e.g. to prune old
/// consumed ledger entries. This is *not* the release consume step (that is
/// `relman changeset consume`, which stamps rather than deletes). Destructive
/// and irreversible, so it is guarded: without `--yes` the command is a dry run
/// that lists what *would* be removed and makes no changes (exit 0). Pass
/// `--yes` to actually delete.
#[derive(ClapArgs)]
pub struct Args {
    /// Actually delete the changesets. Without this flag `clear` is a dry run:
    /// it lists what would be removed and changes nothing.
    #[arg(long)]
    yes: bool,
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangesetCommandError> {
    if !args.yes {
        let pending = ctx.changesets.list()?;
        if pending.is_empty() {
            println!("relman: nothing to clear");
            return Ok(());
        }
        println!(
            "relman: would remove {} changeset(s) (re-run with --yes to delete):",
            pending.len()
        );
        for slug in &pending {
            println!("  {}", slug.file_name());
        }
        return Ok(());
    }

    let removed = ctx.changesets.clear()?;
    if removed.is_empty() {
        println!("relman: nothing to clear");
        return Ok(());
    }
    println!("relman: removed {} changeset(s):", removed.len());
    for slug in &removed {
        println!("  {}", slug.file_name());
    }
    Ok(())
}
