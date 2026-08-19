use clap::{Args as ClapArgs, Subcommand};

use relman_core::ports::ChangesetsError;
use relman_core::types::EmptyDescription;

use crate::context::Ctx;

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
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), ChangesetCommandError> {
    match &args.action {
        Action::New(new_args) => new::run(new_args, ctx),
    }
}
