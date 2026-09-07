use clap::Args as ClapArgs;

use relman_core::ports::Violation;

use crate::commands::changeset::{ChangesetCommandError, DEFAULT_BASE};
use crate::context::Ctx;

/// `relman changeset check [--base <REF>]`.
#[derive(ClapArgs)]
pub struct Args {
    /// The base ref to diff `HEAD` against (the PR's merge target).
    #[arg(long, value_name = "REF", default_value = DEFAULT_BASE)]
    base: String,
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangesetCommandError> {
    let report = ctx.changeset_check.check(&args.base)?;
    if report.is_ok() {
        println!("relman: changeset check ok");
        return Ok(());
    }
    for violation in &report.violations {
        eprintln!("relman: {}", violation.message());
    }
    let count = report.violations.len();
    let malformed = report.violations.iter().any(|violation| {
        matches!(
            violation,
            Violation::ChangesetParse { .. } | Violation::UnknownTargetInChangeset(_)
        )
    });
    if malformed {
        Err(ChangesetCommandError::CheckMalformed { count })
    } else {
        Err(ChangesetCommandError::CheckFailed { count })
    }
}
