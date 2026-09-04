use clap::Args as ClapArgs;

use relman_core::ports::{ApplyError, DeriveError};

use crate::context::Ctx;
use crate::format;

/// `relman bump [--dry-run]` — derive the per-crate bump table and apply it to
/// the workspace manifests.
#[derive(ClapArgs)]
pub struct Args {
    /// Print the derived bump table without editing any files.
    #[arg(long)]
    dry_run: bool,
}

/// What can go wrong running `relman bump`.
#[derive(Debug, thiserror::Error)]
pub enum BumpCommandError {
    /// Deriving the table failed (bad changeset, unknown target, workspace or
    /// store I/O).
    #[error("version derivation failed")]
    Derive(#[from] DeriveError),
    /// Applying the table to the manifests failed.
    #[error("applying the bump failed")]
    Apply(#[from] ApplyError),
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), BumpCommandError> {
    crate::warn::unfilled_templates(ctx);
    let table = ctx.versions.derive()?;

    // Nothing to do, and nothing to render beyond the "nothing bumps" note —
    // the same whether or not `--dry-run` was passed.
    if table.is_empty() {
        print!("{}", format::bump_table(&table));
        return Ok(());
    }

    // The derived table is the summary in both modes; the header distinguishes
    // "would apply" (dry run, no writes) from "applied" (edits performed).
    print!("{}", format::bump_table(&table));

    if args.dry_run {
        println!(
            "\ndry run: would bump {} crate(s) and update pins in {} — no files changed",
            table.len(),
            ctx.root_manifest.display(),
        );
        return Ok(());
    }

    ctx.apply_bump.apply(&table)?;
    println!(
        "\napplied: bumped {} crate(s) and updated matching pins in {}",
        table.len(),
        ctx.root_manifest.display(),
    );
    Ok(())
}
