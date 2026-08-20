use std::path::PathBuf;

use clap::Args as ClapArgs;

use relman_core::ports::ArtifactError;
use relman_core::types::{CycleId, CycleStatus, CycleStatusError, InvalidCycleId};

use crate::context::Ctx;

/// `relman pr-body --cycle <ID> [--status <FILE>]` — render the release-PR
/// markdown to stdout.
///
/// With `--status`, the body is enriched into a live dashboard (gate
/// watermarks, release candidates, per-target tag column); without it, the
/// plain derivation view.
#[derive(ClapArgs)]
pub struct Args {
    /// The release-cycle identifier (e.g. `2026-08-15`).
    #[arg(long)]
    cycle: String,
    /// Path to a cycle-status TOML file describing the pipeline's live git
    /// state (gate watermarks, release candidates, released cycle). Omitted →
    /// the plain derivation view.
    #[arg(long)]
    status: Option<PathBuf>,
}

/// What can go wrong running `relman pr-body`.
#[derive(Debug, thiserror::Error)]
pub enum PrBodyCommandError {
    /// The `--cycle` value was not a valid cycle id.
    #[error("invalid --cycle value")]
    Cycle(#[from] InvalidCycleId),
    /// The `--status` file could not be read.
    #[error("could not read the --status file {path:?}")]
    StatusRead {
        /// The offending path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The `--status` file was not a valid cycle-status document.
    #[error("could not parse the --status file {path:?}")]
    StatusParse {
        /// The offending path.
        path: PathBuf,
        /// Why it was rejected.
        #[source]
        source: CycleStatusError,
    },
    /// Rendering the PR body failed.
    #[error("could not render the release-PR body")]
    Artifact(#[from] ArtifactError),
}

pub fn run(args: &Args, ctx: &Ctx) -> Result<(), PrBodyCommandError> {
    let cycle = CycleId::parse(&args.cycle)?;
    crate::warn::unfilled_templates(ctx);

    let status = match &args.status {
        Some(path) => {
            let raw =
                std::fs::read_to_string(path).map_err(|source| PrBodyCommandError::StatusRead {
                    path: path.clone(),
                    source,
                })?;
            Some(CycleStatus::parse_toml(&raw).map_err(|source| {
                PrBodyCommandError::StatusParse {
                    path: path.clone(),
                    source,
                }
            })?)
        }
        None => None,
    };

    let body = ctx.release_artifacts.pr_body(&cycle, status.as_ref())?;
    print!("{body}");
    Ok(())
}
