use clap::{Parser, Subcommand};

use crate::commands::{about, changeset, versions};

#[derive(Parser)]
#[command(
    name = "relman",
    about = "Zaino release manager: changeset-driven versioning, changelogs, and release orchestration"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Report relman's version and the current date/time
    About(about::Args),
    /// Author and manage changeset files
    Changeset(changeset::Args),
    /// Derive and print the per-crate version bump table
    Versions(versions::Args),
}
