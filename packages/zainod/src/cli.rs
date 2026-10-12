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
    /// Check every file each enabled index has committed against its page checksums.
    ///
    /// Read-only and safe beside a running zainod: it opens nothing for writing, creates no
    /// directory and takes no lock. Each file is checked up to its committed length; bytes a
    /// live writer appended past it are reported as orphaned, not as corruption.
    ///
    /// Prints the full report as JSON on stdout and a summary on stderr. Exits 0 when every
    /// page is intact, 1 when any file is lost or corrupt, and 2 when the indexes could not be
    /// read at all.
    Verify {
        /// Path to the configuration file. Defaults to $XDG_CONFIG_HOME/zaino/zainod.toml
        #[arg(short, long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Take this node out of rotation until `zainod resume`, across restarts and reboots.
    ///
    /// Writes a marker in `[serve] state_dir`. The running zainod picks it up within a second:
    /// its state becomes `held` (`/readyz` 503), and after `[grpc.shutdown]`'s delay every open
    /// gRPC connection is sent GOAWAY. It keeps answering health and admin requests. A stopped
    /// zainod is never reported held.
    ///
    /// Prints `held`, then on stderr what changed and, when `[metrics] listen_address` is set,
    /// the state the running zainod reports. Exits 0 once the marker is in place (holding twice
    /// is a no-op), 1 when the config or state directory could not be read or written.
    Hold {
        /// Path to the configuration file. Defaults to $XDG_CONFIG_HOME/zaino/zainod.toml
        #[arg(short, long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Undo `zainod hold`: back to syncing or serving as the indexes dictate.
    ///
    /// Removes the marker in `[serve] state_dir`; the running zainod picks it up within a
    /// second. Prints `resumed`, then on stderr what changed and the running zainod's state.
    /// Exits 0 once the marker is gone (resuming when not held is a no-op), 1 on an error.
    Resume {
        /// Path to the configuration file. Defaults to $XDG_CONFIG_HOME/zaino/zainod.toml
        #[arg(short, long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
}

impl Command {
    /// Generate a configuration file with default values.
    pub fn generate_config(output: Option<PathBuf>) {
        let path = output.unwrap_or_else(crate::paths::default_config);

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
