//! Validate every record in the ledger and keep the root README's index in step.
#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut write = false;
    let mut root = PathBuf::from(".");
    for arg in env::args().skip(1) {
        if arg == "--write" {
            write = true;
        } else {
            root = PathBuf::from(arg);
        }
    }
    let records = match workbench::check(&root) {
        Ok(records) => records,
        Err(violations) => {
            eprint!("{violations}");
            return ExitCode::FAILURE;
        }
    };
    match workbench::sync_readme(&root, &records, write) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) if write => {
            println!("README.md index regenerated");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            eprintln!("README.md index is stale; run `check-ledger --write`");
            ExitCode::FAILURE
        }
        Err(why) => {
            eprintln!("{why}");
            ExitCode::FAILURE
        }
    }
}
