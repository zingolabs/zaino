use clap::{Parser, Subcommand};

use crate::commands::{about, bump, changelog, changeset, pr_body, publish_plan, tags, versions};

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
    /// Apply the derived version bumps to the workspace manifests
    Bump(bump::Args),
    /// Generate changelog entries for each bumping crate and the workspace
    Changelog(changelog::Args),
    /// Print the git tag plan for a release cycle
    Tags(tags::Args),
    /// Render the release-PR body markdown
    PrBody(pr_body::Args),
    /// Print the bumping crates in publish (dependency) order
    PublishPlan(publish_plan::Args),
}
