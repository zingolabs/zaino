use clap::Args as ClapArgs;

use relman_core::ports::{ChangelogEdit, ChangelogGenError};

use crate::context::Ctx;

/// `relman changelog [--dry-run]` — generate Keep-a-Changelog entries for each
/// bumping crate and the workspace, from the accumulated changesets.
#[derive(ClapArgs)]
pub struct Args {
    /// Print the planned edits (file + inserted section) without writing.
    #[arg(long)]
    dry_run: bool,
}

/// What can go wrong running `relman changelog`.
#[derive(Debug, thiserror::Error)]
pub enum ChangelogCommandError {
    /// Generating or writing the changelog edits failed.
    #[error("changelog generation failed")]
    Generate(#[from] ChangelogGenError),
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangelogCommandError> {
    crate::warn::unfilled_templates(ctx);
    let edits = if args.dry_run {
        ctx.changelog.generate()?
    } else {
        ctx.changelog.apply()?
    };

    if edits.is_empty() {
        println!("relman: nothing to write (no changesets affect a governed target)");
        return Ok(());
    }

    print!("{}", render_edits(&edits, args.dry_run));
    Ok(())
}

/// Render the planned edits for the terminal: each file path followed by the
/// section that would be (or was) inserted, then a one-line summary.
fn render_edits(edits: &[ChangelogEdit], dry_run: bool) -> String {
    let mut out = String::new();
    for edit in edits {
        let verb = if dry_run { "would update" } else { "updated" };
        out.push_str(&format!("{} {}\n", verb, edit.path().display()));
        for line in edit.inserted().lines() {
            out.push_str(&format!("    {line}\n"));
        }
    }
    let summary = if dry_run {
        format!(
            "\ndry run: would update {} changelog(s) — no files changed\n",
            edits.len()
        )
    } else {
        format!("\napplied: updated {} changelog(s)\n", edits.len())
    };
    out.push_str(&summary);
    out
}
