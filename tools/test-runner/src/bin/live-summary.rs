//! `live-summary` — run the test partitions and print a combined summary.
//!
//! Invoked from `makers test live` as `cargo run --bin live-summary -- <args>`
//! and from `makers test all` with the extra `--all` flag. The live set runs
//! the `clientless` then `e2e` partition (each in its own CI container via its
//! own `makers` task); `--all` first runs the `packages` set (`makers
//! container-test`) and adds its row to the table. The runner streams each
//! run's output while capturing it, parses the nextest summary line for
//! tallies, and measures wall-clock duration via `Instant`.
//!
//! Unlike a cargo-make `dependencies` list (which is fail-fast), this runs BOTH
//! partitions even when the first fails, so the summary reflects the whole
//! suite. It then exits non-zero if either partition failed, so CI still
//! catches it.
//!
//! `--with-zcashd` is forwarded as a flag to the child `makers` calls so the
//! zcashd-backed tests are included; otherwise nothing enables them.
#![forbid(unsafe_code)]

use std::error::Error;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Which partitions this invocation runs and summarizes.
enum TestSet {
    /// The two live partitions (`makers test live`).
    Live,
    /// The packages set plus the two live partitions (`makers test all`).
    All,
}

/// One nextest run's tallies, zero where the summary line was absent.
#[derive(Default)]
struct Summary {
    run: u64,
    passed: u64,
    failed: u64,
    skipped: u64,
    /// Wall-clock duration measured by the runner (includes build + test time).
    elapsed: Duration,
}

impl Summary {
    fn add(&self, other: &Summary) -> Summary {
        Summary {
            run: self.run + other.run,
            passed: self.passed + other.passed,
            failed: self.failed + other.failed,
            skipped: self.skipped + other.skipped,
            elapsed: self.elapsed + other.elapsed,
        }
    }
}

/// Run one partition's `makers` task, streaming its combined output to our
/// stdout while capturing it for parsing. Returns (exit_code, captured_output).
fn run_partition(task: &str, with_zcashd: bool) -> Result<(i32, String), Box<dyn Error>> {
    // `bash -c '... 2>&1'` merges stderr into stdout so the single captured
    // stream carries the nextest summary line wherever nextest emits it.
    let mut cmd = Command::new("bash");
    // Forward `--with-zcashd` as an explicit flag (no implicit env var): the
    // partition task passes it through to `container-test`, which adds
    // `--features zcashd_support`.
    let zcashd_flag = if with_zcashd { " --with-zcashd" } else { "" };
    cmd.arg("-c").arg(format!("makers {task}{zcashd_flag} 2>&1"));
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

/// Remove ANSI CSI escape sequences (`ESC [ … <final byte>`) so the digits in a
/// summary line aren't split by colour codes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Consume up to and including the final byte (0x40..=0x7E).
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            // A lone ESC with no '[' is just dropped.
        } else {
            out.push(c);
        }
    }
    out
}

/// The integer immediately preceding `marker` (after optional spaces), or 0 if
/// `marker` is absent. e.g. `count_before("... 8 passed", "passed") == 8`.
fn count_before(line: &str, marker: &str) -> u64 {
    let Some(idx) = line.find(marker) else {
        return 0;
    };
    let head = line[..idx].trim_end();
    let digit_count = head.chars().rev().take_while(|c| c.is_ascii_digit()).count();
    head[head.len() - digit_count..].parse().unwrap_or(0)
}

/// Parse the last nextest summary line out of a captured run.
///
/// nextest prints e.g.:
///   Summary [ 73.207s] 8 tests run: 8 passed (2 slow), 2 skipped
///   Summary [510.718s] 29 tests run: 23 passed (14 slow), 6 failed, 2 skipped
///   Summary [  1.795s] 1 test run: 0 passed, 1 failed, 114 skipped   (singular)
fn parse_summary(log: &str) -> Summary {
    // Strip ANSI, then take the last "N test(s) run:" line nextest emitted.
    let line = log
        .lines()
        .map(strip_ansi)
        .rfind(|l| l.contains("run:") && l.contains("test"))
        .unwrap_or_default();

    Summary {
        // The run count is the integer before the word "test" ("N tests run:").
        run: count_before(&line, "test"),
        passed: count_before(&line, "passed"),
        failed: count_before(&line, "failed"),
        skipped: count_before(&line, "skipped"),
        ..Summary::default()
    }
}

fn print_row(label: &str, s: &Summary) {
    let total_secs = s.elapsed.as_secs();
    let millis = s.elapsed.subsec_millis();
    let secs = format!("{total_secs}.{millis:03}");
    println!(
        "  {label:<18} {:>4} run, {:>4} passed, {:>4} failed, {:>4} skipped, {secs:>10}s",
        s.run, s.passed, s.failed, s.skipped
    );
}

fn main() -> Result<(), Box<dyn Error>> {
    let with_zcashd = std::env::args().any(|a| a == "--with-zcashd");
    let set = if std::env::args().any(|a| a == "--all") {
        TestSet::All
    } else {
        TestSet::Live
    };

    let packages = match set {
        TestSet::All => {
            println!(">>> all: running packages set");
            let start = Instant::now();
            let (rc, log) = run_partition("container-test", with_zcashd)?;
            let elapsed = start.elapsed();
            let mut s = parse_summary(&log);
            s.elapsed = elapsed;
            Some((rc, s))
        }
        TestSet::Live => None,
    };

    println!(">>> live: running clientless partition");
    let cl_start = Instant::now();
    let (cl_rc, cl_log) = run_partition("live-clientless", with_zcashd)?;
    let cl_elapsed = cl_start.elapsed();

    println!(">>> live: running e2e partition");
    let e2e_start = Instant::now();
    let (e2e_rc, e2e_log) = run_partition("live-e2e", with_zcashd)?;
    let e2e_elapsed = e2e_start.elapsed();

    let mut cl = parse_summary(&cl_log);
    cl.elapsed = cl_elapsed;
    let mut e2e = parse_summary(&e2e_log);
    e2e.elapsed = e2e_elapsed;

    let header = match packages {
        Some(_) => "test summary",
        None => "live summary",
    };
    let mut total = cl.add(&e2e);
    if let Some((_, p)) = &packages {
        total = total.add(p);
    }

    println!();
    println!("====================== {header} ==========================");
    if let Some((_, p)) = &packages {
        print_row("packages:", p);
    }
    print_row("clientless:", &cl);
    print_row("e2e:", &e2e);
    print_row("TOTAL:", &total);
    println!("==============================================================");

    // A partition that errored without producing a summary line likely failed
    // to build; call it out so the zeros above aren't read as "all clear".
    if let Some((pkg_rc, p)) = &packages {
        if *pkg_rc != 0 && p.run == 0 {
            println!("  warning: packages produced no nextest summary (build failure?) — see output above.");
        }
    }
    if cl_rc != 0 && cl.run == 0 {
        println!("  warning: clientless produced no nextest summary (build failure?) — see output above.");
    }
    if e2e_rc != 0 && e2e.run == 0 {
        println!("  warning: e2e produced no nextest summary (build failure?) — see output above.");
    }

    // Fail the front door if any set failed.
    let pkg_rc = packages.map_or(0, |(rc, _)| rc);
    if pkg_rc != 0 || cl_rc != 0 || e2e_rc != 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod parse_summary {
    use super::*;

    fn check(line: &str, run: u64, passed: u64, failed: u64, skipped: u64) {
        let s = parse_summary(line);
        assert_eq!((s.run, s.passed, s.failed, s.skipped), (run, passed, failed, skipped));
    }

    #[test]
    fn plural_no_failures() {
        check("Summary [ 73.207s] 8 tests run: 8 passed (2 slow), 2 skipped", 8, 8, 0, 2);
    }

    #[test]
    fn plural_with_failures() {
        check(
            "Summary [510.718s] 29 tests run: 23 passed (14 slow), 6 failed, 2 skipped",
            29, 23, 6, 2,
        );
    }

    #[test]
    fn singular() {
        check("Summary [  1.795s] 1 test run: 0 passed, 1 failed, 114 skipped", 1, 0, 1, 114);
    }

    #[test]
    fn strips_ansi_color_codes() {
        let colored = "\x1b[1m\x1b[32mSummary\x1b[0m [73s] \x1b[1m8\x1b[0m tests run: 8 passed, 2 skipped";
        check(colored, 8, 8, 0, 2);
    }

    #[test]
    fn missing_summary_is_all_zero() {
        check("no summary line here", 0, 0, 0, 0);
    }

    #[test]
    fn takes_the_last_summary_line() {
        let log = "Summary [1s] 1 test run: 1 passed, 0 skipped\n\
                   Summary [2s] 9 tests run: 7 passed, 1 failed, 1 skipped";
        check(log, 9, 7, 1, 1);
    }
}
