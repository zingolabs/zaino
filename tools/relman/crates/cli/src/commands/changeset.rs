use clap::{Args as ClapArgs, Subcommand};

use relman_core::ports::{ChangesetsError, CheckError};
use relman_core::types::EmptyDescription;

use crate::context::Ctx;

mod check;
mod new;

/// `relman changeset <action>` — author and manage changeset files.
#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Scaffold a new changeset file under `.changesets/`
    New(new::Args),
    /// Enforce that a PR touching governed source carries a covering changeset
    Check(check::Args),
}

/// What can go wrong running a `changeset` subcommand.
#[derive(Debug, thiserror::Error)]
pub enum ChangesetCommandError {
    /// The `--empty` reason was blank.
    #[error("--empty requires a non-empty reason")]
    EmptyReason(#[from] EmptyDescription),
    /// The changeset could not be created.
    #[error(transparent)]
    Changesets(#[from] ChangesetsError),
    /// The check ran and found the PR non-compliant. The per-violation
    /// diagnostics were already written to stderr; this carries the exit-code
    /// summary.
    #[error("changeset check failed: {count} violation(s)")]
    CheckFailed {
        /// How many violations were reported.
        count: usize,
    },
    /// The check could not run because of an infrastructure failure.
    #[error("changeset check could not run")]
    Check(#[from] CheckError),
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangesetCommandError> {
    match &args.action {
        Action::New(new_args) => new::run(new_args, ctx),
        Action::Check(check_args) => check::run(check_args, ctx),
    }
}
