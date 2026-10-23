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
    /// Generate a configuration file with default values.
    GenerateConfig {
        /// Output path for the generated config file. Defaults to the --config path
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
}
