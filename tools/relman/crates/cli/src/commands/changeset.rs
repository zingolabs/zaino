use clap::{Args as ClapArgs, Subcommand};

use relman_core::ports::{ChangesetsError, CheckError};
use relman_core::types::{EmptyDescription, InvalidCycleId};

use crate::context::Ctx;

mod check;
mod clear;
mod consume;
mod new;
mod rename;

/// The default base ref: PRs are gated against `dev`.
const DEFAULT_BASE: &str = "dev";

/// The exit status for a check that found the PR non-compliant.
const EXIT_CHECK_FAILED: u8 = 1;

/// The exit status for a check that found a malformed changeset, so CI can
/// always fail it regardless of the advisory rollout flag.
const EXIT_CHECK_MALFORMED: u8 = 2;

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
    /// Rename this PR's author changeset(s) to the canonical `pr-<N>` name(s)
    Rename(rename::Args),
    /// Mark every pending changeset consumed by a cycle, in place (the release
    /// consume step; needs `--yes`)
    Consume(consume::Args),
    /// Remove every changeset file (a manual GC for old ledger entries; needs
    /// `--yes`)
    Clear(clear::Args),
}

/// What can go wrong running a `changeset` subcommand.
#[derive(Debug, thiserror::Error)]
pub enum ChangesetCommandError {
    /// The `--empty` reason was blank.
    #[error("--empty requires a non-empty reason")]
    EmptyReason(#[from] EmptyDescription),
    /// The `--cycle` value was not a valid cycle id.
    #[error("invalid --cycle value")]
    Cycle(#[from] InvalidCycleId),
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
    /// The check ran and at least one of this PR's changesets is malformed
    /// (unparseable, or naming an unknown target), which no rollout flag may
    /// downgrade to a warning.
    #[error("changeset check failed: {count} violation(s), including a malformed changeset")]
    CheckMalformed {
        /// How many violations were reported.
        count: usize,
    },
    /// The check could not run because of an infrastructure failure.
    #[error("changeset check could not run")]
    Check(#[from] CheckError),
}

impl ChangesetCommandError {
    /// The process exit status this error maps to.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::CheckMalformed { .. } => EXIT_CHECK_MALFORMED,
            _ => EXIT_CHECK_FAILED,
        }
    }
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangesetCommandError> {
    match &args.action {
        Action::New(new_args) => new::run(new_args, ctx),
        Action::Check(check_args) => check::run(check_args, ctx),
        Action::Rename(rename_args) => rename::run(rename_args, ctx),
        Action::Consume(consume_args) => consume::run(consume_args, ctx),
        Action::Clear(clear_args) => clear::run(clear_args, ctx),
    }
}
