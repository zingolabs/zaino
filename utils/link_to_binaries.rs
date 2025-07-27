#!/usr/bin/env rust-script
//! ```cargo
//! [dependencies]
//! clap = { version = "4.0", features = ["derive"] }
//! symlistow = { git = "https://github.com/zancas/rust_scripting/symlistow.git" }
//! ```

use clap::Parser;
use std::path::PathBuf;
use std::process::{exit, Command};
use symlistow::{handle_symlink, verify_and_push};

#[derive(Parser, Debug)]
#[command(name = "link_to_binaries")]
#[command(about = "Creates symlinks for test binaries and optionally executes commands")]
struct Args {
    /// Repository root directory
    #[arg(long = "zaino_rootdir", default_value = "/home/container_user/zaino")]
    zaino_rootdir: String,

    /// Path to zcashd binary
    #[arg(
        long = "zcashd_bin",
        default_value = "/home/container_user/artifacts/zcashd"
    )]
    zcashd_bin: String,

    /// Path to zcash-cli binary
    #[arg(
        long = "zcashcli_bin",
        default_value = "/home/container_user/artifacts/zcash-cli"
    )]
    zcashcli_bin: String,

    /// Path to zebrad binary
    #[arg(
        long = "zebrad_bin",
        default_value = "/home/container_user/artifacts/zebrad"
    )]
    zebrad_bin: String,

    /// Interactive mode - prompt for replacements (default: true, disable for CI)
    #[arg(long = "interactive", default_value = "true")]
    interactive: bool,

    /// Command and arguments to execute after setting up symlinks
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

fn main() {
    let args = Args::parse();

    // First, verify all source binaries exist and work
    println!("Verifying source binaries...");

    let mut versions = Vec::new();

    let mut all_valid = true;
    all_valid &= verify_and_push(&args.zcashd_bin, "zcashd", &mut versions);
    all_valid &= verify_and_push(&args.zebrad_bin, "zebrad", &mut versions);
    all_valid &= verify_and_push(&args.zcashcli_bin, "zcash-cli", &mut versions);

    if !all_valid {
        eprintln!("\nError: Not all required binaries are valid. Please check the paths and ensure the binaries are executable.");
        exit(1);
    }

    println!("\nAll source binaries verified successfully!");

    if !args.interactive {
        println!("Running in non-interactive mode (CI)");
    }

    // Check if test_binaries/bins directory exists and create symlinks if binaries are missing
    let bins_dir = PathBuf::from(&args.zaino_rootdir).join("test_binaries/bins");

    if bins_dir.exists() {
        println!("\nSetting up symlinks in {}...", bins_dir.display());

        // Process each binary
        for (name, path, version) in versions {
            let link_path = bins_dir.join(&name);
            handle_symlink(&link_path, &path, &name, &version, args.interactive);
        }

        println!(
            "\nBinary setup complete. Contents of {}:",
            bins_dir.display()
        );
        if let Ok(entries) = std::fs::read_dir(&bins_dir) {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    let file_type = if metadata.is_symlink() {
                        "symlink"
                    } else if metadata.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    println!("  {} ({})", entry.file_name().to_string_lossy(), file_type);
                }
            }
        }
    } else {
        println!(
            "Info: {} will be created when binaries are first accessed",
            bins_dir.display()
        );
    }

    // Execute command if provided
    if !args.command.is_empty() {
        let status = Command::new(&args.command[0])
            .args(&args.command[1..])
            .status()
            .unwrap_or_else(|e| {
                eprintln!("Failed to execute command '{}': {}", args.command[0], e);
                exit(1);
            });

        exit(status.code().unwrap_or(1));
    }
}
