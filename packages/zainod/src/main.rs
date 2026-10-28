//! Zaino Indexer daemon.

use clap::Parser;

use zainodlib::cli::{default_config_path, Cli, Command};

#[tokio::main]
async fn main() {
    zaino_common::logging::init();

    let cli = Cli::parse();

    match cli.command {
        Command::Start { config } => {
            let config_path = config.unwrap_or_else(default_config_path);
            if let Err(e) = zainodlib::run(config_path).await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::GenerateConfig { output } => Command::generate_config(output),
    }
}
