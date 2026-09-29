//! Zaino Indexer daemon.

use clap::Parser;

use zainodlib::cli::{Cli, Command};

#[tokio::main]
async fn main() {
    // a panic anywhere = a violated invariant: print it, then die (never unwound into a task
    // boundary that keeps serving; docs/design/durability.md §6)
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));

    let cli = Cli::parse();

    // logging → stdout by default (reserved for `verify`'s JSON)
    if !matches!(cli.command, Command::Verify { .. }) {
        if let Err(error) = zainodlib::logging::init() {
            eprintln!("Error: logging: {error}");
            std::process::exit(2);
        }
    }

    match cli.command {
        Command::Start { config } => {
            let config_path = config.unwrap_or_else(zainodlib::paths::default_config);
            if let Err(e) = zainodlib::run(config_path).await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::GenerateConfig { output } => Command::generate_config(output),
        Command::Verify { config } => {
            let config_path = config.unwrap_or_else(zainodlib::paths::default_config);
            std::process::exit(zainodlib::verify::run(&config_path));
        }
    }
}
