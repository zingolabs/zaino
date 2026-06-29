#!/usr/bin/env rust-script
//! Run both live-test partitions and print a combined pass/fail summary.
//!
//! Used as the script of the `live-tests` front door. Runs the `integration`
//! then `e2e` partition (each in its own CI container via its own `makers`
//! task), streams each run's output while capturing it, parses the nextest
//! summary line, and aggregates the totals.
//!
//! Unlike a cargo-make `dependencies` list (which is fail-fast), this runs BOTH
//! partitions even when the first fails, so the summary reflects the whole
//! suite. It then exits non-zero if either partition failed, so CI still
//! catches it.
//!
//! `--with-zcashd` is forwarded to the child `makers` calls (as the
//! `CONTAINER_TEST_WITH_ZCASHD` env var the engines honour) so the zcashd-backed
//! tests are included.
//!
//! ```cargo
//! [dependencies]
//! regex = "1"
//! ```
#![forbid(unsafe_code)]

use std::error::Error;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use regex::Regex;

/// One nextest run's tallies, zero where the summary line was absent.
#[derive(Default)]
struct Summary {
    run: u64,
    passed: u64,
    failed: u64,
    skipped: u64,
}

impl Summary {
    fn add(&self, other: &Summary) -> Summary {
        Summary {
            run: self.run + other.run,
            passed: self.passed + other.passed,
            failed: self.failed + other.failed,
            skipped: self.skipped + other.skipped,
        }
    }
}

/// Run one partition's `makers` task, streaming its combined output to our
/// stdout while capturing it for parsing. Returns (exit_code, captured_output).
fn run_partition(task: &str, with_zcashd: bool) -> Result<(i32, String), Box<dyn Error>> {
    // `bash -c '... 2>&1'` merges stderr into stdout so the single captured
    // stream carries the nextest summary line wherever nextest emits it.
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(format!("makers {task} 2>&1"));
    if with_zcashd {
        cmd.env("CONTAINER_TEST_WITH_ZCASHD", "1");
    }
    let mut child = cmd.stdout(Stdio::piped()).spawn()?;

    let stdout = child
        .stdout
        .take()
        .expect("child stdout is piped: Stdio::piped() was set above");
    let mut captured = String::new();
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        println!("{line}");
        captured.push_str(&line);
        captured.push('\n');
    }

    let code = child.wait()?.code().unwrap_or(1);
    Ok((code, captured))
}

/// Parse the last nextest summary line out of a captured run.
///
/// nextest prints e.g.:
///   Summary [ 73.207s] 8 tests run: 8 passed (2 slow), 2 skipped
///   Summary [510.718s] 29 tests run: 23 passed (14 slow), 6 failed, 2 skipped
///   Summary [  1.795s] 1 test run: 0 passed, 1 failed, 114 skipped   (singular)
fn parse_summary(log: &str) -> Summary {
    let ansi = Regex::new(r"\x1b\[[0-9;]*m").expect("static ANSI-escape regex compiles");
    let run_re = Regex::new(r"(\d+) tests? run:").expect("static run-count regex compiles");
    let passed_re = Regex::new(r"(\d+) passed").expect("static passed-count regex compiles");
    let failed_re = Regex::new(r"(\d+) failed").expect("static failed-count regex compiles");
    let skipped_re = Regex::new(r"(\d+) skipped").expect("static skipped-count regex compiles");

    // Strip ANSI, then take the last "N test(s) run:" line nextest emitted.
    let line = log
        .lines()
        .map(|l| ansi.replace_all(l, "").into_owned())
        .filter(|l| run_re.is_match(l))
        .last()
        .unwrap_or_default();

    let field = |re: &Regex| -> u64 {
        re.captures(&line)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0)
    };

    Summary {
        run: field(&run_re),
        passed: field(&passed_re),
        failed: field(&failed_re),
        skipped: field(&skipped_re),
    }
}

fn print_row(label: &str, s: &Summary) {
    println!(
        "  {label:<18} {:>4} run, {:>4} passed, {:>4} failed, {:>4} skipped",
        s.run, s.passed, s.failed, s.skipped
    );
}

fn main() -> Result<(), Box<dyn Error>> {
    let with_zcashd = std::env::args().any(|a| a == "--with-zcashd");

    println!(">>> live: running integration partition");
    let (int_rc, int_log) = run_partition("live-integration", with_zcashd)?;

    println!(">>> live: running e2e partition");
    let (e2e_rc, e2e_log) = run_partition("live-e2e", with_zcashd)?;

    let int = parse_summary(&int_log);
    let e2e = parse_summary(&e2e_log);

    println!();
    println!("====================== live summary ==========================");
    print_row("integration:", &int);
    print_row("e2e:", &e2e);
    print_row("TOTAL:", &int.add(&e2e));
    println!("==============================================================");

    // A partition that errored without producing a summary line likely failed
    // to build; call it out so the zeros above aren't read as "all clear".
    if int_rc != 0 && int.run == 0 {
        println!("  warning: integration produced no nextest summary (build failure?) — see output above.");
    }
    if e2e_rc != 0 && e2e.run == 0 {
        println!("  warning: e2e produced no nextest summary (build failure?) — see output above.");
    }

    // Fail the front door if either partition failed.
    if int_rc != 0 || e2e_rc != 0 {
        std::process::exit(1);
    }
    Ok(())
}
