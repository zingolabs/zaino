//! Command-line interface for Zaino.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use zaino_common::xdg::resolve_path_with_xdg_config_defaults;

/// Returns the default config path following XDG Base Directory spec.
///
/// Uses `$XDG_CONFIG_HOME/zaino/zainod.toml` if set,
/// otherwise falls back to `$HOME/.config/zaino/zainod.toml`,
/// or `/tmp/zaino/.config/zaino/zainod.toml` if HOME is unset.
pub fn default_config_path() -> PathBuf {
    resolve_path_with_xdg_config_defaults("zaino/zainod.toml")
}

/// The Zcash Indexing Service.
#[derive(Parser, Debug)]
#[command(
    name = "zainod",
    version,
    about = "Zaino - The Zcash Indexing Service",
    long_about = None
)]
pub struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Command,
}

/// Available subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Start the Zaino indexer service.
    Start {
        /// Path to the configuration file. Defaults to $XDG_CONFIG_HOME/zaino/zainod.toml
        #[arg(short, long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Generate a configuration file with default values.
    GenerateConfig {
        /// Output path for the generated config file. Defaults to $XDG_CONFIG_HOME/zaino/zainod.toml
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
}

impl Command {
    /// Generate a configuration file with default values.
    pub fn generate_config(output: Option<PathBuf>) {
        let path = output.unwrap_or_else(default_config_path);

        let content = match crate::config::generate_default_config() {
            Ok(content) => content,
            Err(e) => {
                eprintln!("Error generating config: {}", e);
                std::process::exit(1);
            }
        };

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("Error creating directory {}: {}", parent.display(), e);
                    std::process::exit(1);
                }
            }
        }

        if let Err(e) = std::fs::write(&path, &content) {
            eprintln!("Error writing to {}: {}", path.display(), e);
            std::process::exit(1);
        }

        eprintln!("Generated config file: {}", path.display());
    }
}
