//! Command-line interface for Zaino.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// The Zcash Indexing Service.
#[derive(Parser, Debug)]
#[command(
    name = "zainod",
    version,
    about = "Zaino - The Zcash Indexing Service",
    long_about = None
)]
pub(super) struct Cli {
    /// Path to the configuration file. Defaults to $XDG_CONFIG_HOME/zaino/zainod.toml
    #[arg(short, long, value_name = "FILE", global = true)]
    pub(super) config: Option<PathBuf>,
    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub(super) enum Command {
    /// Start the Zaino indexer service.
    Start,
    /// Generate a configuration file with default values.
    GenerateConfig {
        /// Output path for the generated config file. Defaults to the --config path
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Check every file each enabled index has made durable against its page checksums.
    ///
    /// Read-only and safe beside a running zainod: it opens nothing for writing, creates no
    /// directory and takes no lock. Each file is checked up to its durable length. Bytes a
    /// live writer appended past it are reported as orphaned, not as corruption.
    ///
    /// Prints the full report as JSON on stdout and a summary on stderr. Exits 0 when every
    /// page is intact, 1 when any file is lost or corrupt, and 2 when the indexes could not be
    /// read at all.
    Verify,
    #[command(flatten)]
    Hold(crate::command::hold::Action),
    /// Override when this node reports `Overloaded`, across restarts: markers in `[serve]
    /// state_dir`/overload over `[grpc.overload]`, read by the running zainod within a second.
    ///
    /// Prints the settings in force. Exits 0 once the marker matches, 1 on an invalid key or
    /// value, or an unreadable config or state directory.
    Overload {
        #[command(subcommand)]
        action: crate::command::overload::Action,
    },
}
