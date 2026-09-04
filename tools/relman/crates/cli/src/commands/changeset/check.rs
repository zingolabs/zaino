use clap::Args as ClapArgs;

use crate::commands::changeset::ChangesetCommandError;
use crate::context::Ctx;

/// The default base ref: PRs are gated against `dev`.
const DEFAULT_BASE: &str = "dev";

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
    Err(ChangesetCommandError::CheckFailed {
        count: report.violations.len(),
    })
}
