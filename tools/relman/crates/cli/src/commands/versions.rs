use clap::Args as ClapArgs;

use relman_core::ports::DeriveError;

use crate::context::Ctx;
use crate::format;

/// `relman versions` — derive and print the per-crate version bump table.
#[derive(ClapArgs)]
pub struct Args {}

/// What can go wrong running `relman versions`.
#[derive(Debug, thiserror::Error)]
pub enum VersionsCommandError {
    /// The derivation itself failed (bad changeset, unknown target, workspace
    /// or store I/O).
    #[error("version derivation failed")]
    Derive(#[from] DeriveError),
}

pub fn run(_args: &Args, ctx: &Ctx) -> Result<(), VersionsCommandError> {
    let table = ctx.versions.derive()?;
    print!("{}", format::bump_table(&table));
    Ok(())
}
