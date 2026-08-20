use clap::Args as ClapArgs;

use relman_core::types::CycleId;

use crate::commands::changeset::ChangesetCommandError;
use crate::context::Ctx;

/// `relman changeset consume --cycle <ID> [--yes]`.
///
/// The release "consume" step: mark every pending changeset as consumed by the
/// given cycle, stamping `consumed_in` in place and leaving the files on disk as
/// a provenance ledger. Already-consumed changesets are skipped, so the command
/// is idempotent. It is guarded like `clear`: without `--yes` it is a dry run
/// that lists what *would* be consumed and changes nothing (exit 0). Pass
/// `--yes` to actually stamp.
#[derive(ClapArgs)]
pub struct Args {
    /// The release-cycle identifier to stamp into each changeset (e.g.
    /// `2026-08-15`).
    #[arg(long)]
    cycle: String,
    /// Actually stamp the changesets. Without this flag `consume` is a dry run:
    /// it lists what would be consumed and changes nothing.
    #[arg(long)]
    yes: bool,
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangesetCommandError> {
    let cycle = CycleId::parse(&args.cycle)?;

    if !args.yes {
        let pending = ctx.changesets.pending()?;
        if pending.is_empty() {
            println!("relman: nothing to consume");
            return Ok(());
        }
        println!(
            "relman: would consume {} changeset(s) into {cycle} (re-run with --yes to stamp):",
            pending.len()
        );
        for slug in &pending {
            println!("  {}", slug.file_name());
        }
        return Ok(());
    }

    let consumed = ctx.changesets.consume(&cycle)?;
    if consumed.is_empty() {
        println!("relman: nothing to consume");
        return Ok(());
    }
    println!(
        "relman: consumed {} changeset(s) into {cycle}:",
        consumed.len()
    );
    for slug in &consumed {
        println!("  {}", slug.file_name());
    }
    Ok(())
}
