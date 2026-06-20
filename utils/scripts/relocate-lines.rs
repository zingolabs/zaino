#!/usr/bin/env rust-script
//! relocate-lines — move contiguous line ranges from one file into another.
//!
//! A small, dependency-free refactoring aid for the bulk code move that is tedious and
//! error-prone by hand: lift one or more line ranges out of a source file and splice them
//! into a destination file, optionally dropping moved lines that contain a marker (e.g. a
//! now-redundant `#[cfg(...)]` attribute when the destination is already gated).
//!
//! Ranges are 1-indexed, inclusive, and given against the *current* source file. The move
//! is a DRY RUN by default — it prints what it would do and writes nothing until you pass
//! `--apply`. Always eyeball the dry-run summary (and re-derive ranges) before applying;
//! line numbers shift as soon as the file changes.
//!
//! Usage:
//!   relocate-lines --from <src> --to <dst> --ranges A-B[,C-D,...] \
//!       [--strip-contains <substr>]...           # drop moved lines containing <substr>
//!       [--insert append|before-last|before:<substr>]   # default: append
//!       [--apply]                                # actually write (otherwise dry run)
//!
//! `--insert before-last` splices in just above the destination's final line (handy when
//! the destination ends in a closing `}` and you want the content inside that block).
//! `--insert before:<substr>` splices above the last line that contains <substr>.
//!
//! Example (the move this tool was generalised from — relocating the gated accumulator
//! tests into their module's `#[cfg(test)] mod tests`, dropping the per-test cfg):
//!   relocate-lines \
//!     --from packages/zaino-state/src/chain_index/tests/finalised_state/v1.rs \
//!     --to   packages/zaino-state/src/chain_index/finalised_state/finalised_source/v1/tx_out_set_accumulator.rs \
//!     --ranges 929-1076,1106-1292,1294-1402 \
//!     --strip-contains '#[cfg(feature = "gettxoutsetinfo")]' \
//!     --insert before-last --apply
//!
//! ```cargo
//! [dependencies]
//! ```
#![forbid(unsafe_code)]

use std::process::exit;

enum Insert {
    Append,
    BeforeLast,
    Before(String),
}

struct Args {
    from: String,
    to: String,
    ranges: Vec<(usize, usize)>,
    strip: Vec<String>,
    insert: Insert,
    apply: bool,
}

fn fail(msg: &str) -> ! {
    eprintln!("relocate-lines: {msg}");
    exit(1);
}

fn parse_args() -> Args {
    let mut from: Option<String> = None;
    let mut to: Option<String> = None;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut strip: Vec<String> = Vec::new();
    let mut insert = Insert::Append;
    let mut apply = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next = || it.next().unwrap_or_else(|| fail(&format!("{arg} needs a value")));
        match arg.as_str() {
            "--from" => from = Some(next()),
            "--to" => to = Some(next()),
            "--ranges" => {
                for part in next().split(',') {
                    let (a, b) = part
                        .split_once('-')
                        .unwrap_or_else(|| fail("each range must be A-B"));
                    let a: usize = a.trim().parse().unwrap_or_else(|_| fail("bad range start"));
                    let b: usize = b.trim().parse().unwrap_or_else(|_| fail("bad range end"));
                    if a == 0 || b < a {
                        fail("range must be 1-indexed with end >= start");
                    }
                    ranges.push((a, b));
                }
            }
            "--strip-contains" => strip.push(next()),
            "--insert" => {
                let v = next();
                insert = match v.as_str() {
                    "append" => Insert::Append,
                    "before-last" => Insert::BeforeLast,
                    other => match other.strip_prefix("before:") {
                        Some(s) => Insert::Before(s.to_string()),
                        None => fail("--insert must be append|before-last|before:<substr>"),
                    },
                };
            }
            "--apply" => apply = true,
            other => fail(&format!("unknown argument: {other}")),
        }
    }

    if ranges.is_empty() {
        fail("--ranges is required (e.g. --ranges 10-20,40-55)");
    }
    let mut sorted = ranges.clone();
    sorted.sort_unstable();
    for w in sorted.windows(2) {
        if w[0].1 >= w[1].0 {
            fail("ranges must not overlap");
        }
    }

    Args {
        from: from.unwrap_or_else(|| fail("--from is required")),
        to: to.unwrap_or_else(|| fail("--to is required")),
        ranges,
        strip,
        insert,
        apply,
    }
}

fn main() {
    let args = parse_args();

    let src = std::fs::read_to_string(&args.from)
        .unwrap_or_else(|e| fail(&format!("read {}: {e}", args.from)));
    let src_lines: Vec<&str> = src.lines().collect();
    let n = src_lines.len();

    let mut moved: Vec<String> = Vec::new();
    let mut remove = vec![false; n + 1]; // 1-indexed
    for &(a, b) in &args.ranges {
        if b > n {
            fail(&format!("range {a}-{b} exceeds {n} lines in {}", args.from));
        }
        moved.push(String::new()); // blank separator before each moved block
        for ln in a..=b {
            remove[ln] = true;
            let line = src_lines[ln - 1];
            if args.strip.iter().any(|s| line.contains(s.as_str())) {
                continue;
            }
            moved.push(line.to_string());
        }
    }

    let remaining: Vec<&str> = (1..=n)
        .filter(|i| !remove[*i])
        .map(|i| src_lines[i - 1])
        .collect();

    let dst = std::fs::read_to_string(&args.to)
        .unwrap_or_else(|e| fail(&format!("read {}: {e}", args.to)));
    let dst_lines: Vec<&str> = dst.lines().collect();
    let insert_at = match &args.insert {
        Insert::Append => dst_lines.len(),
        Insert::BeforeLast => dst_lines
            .len()
            .checked_sub(1)
            .unwrap_or_else(|| fail("--to is empty; cannot use before-last")),
        Insert::Before(s) => dst_lines
            .iter()
            .rposition(|l| l.contains(s.as_str()))
            .unwrap_or_else(|| fail(&format!("no line in --to contains {s:?}"))),
    };
    let mut new_dst: Vec<String> = dst_lines[..insert_at].iter().map(|s| s.to_string()).collect();
    new_dst.extend(moved.iter().cloned());
    new_dst.extend(dst_lines[insert_at..].iter().map(|s| s.to_string()));

    println!(
        "from {}: {n} -> {} lines  (moving {} content line(s) across {} range(s))",
        args.from,
        remaining.len(),
        moved.iter().filter(|l| !l.is_empty()).count(),
        args.ranges.len(),
    );
    println!(
        "to   {}: {} -> {} lines  (inserted at line index {insert_at})",
        args.to,
        dst_lines.len(),
        new_dst.len(),
    );

    if !args.apply {
        println!("\n(dry run — pass --apply to write the changes)");
        return;
    }

    std::fs::write(&args.from, remaining.join("\n") + "\n")
        .unwrap_or_else(|e| fail(&format!("write {}: {e}", args.from)));
    std::fs::write(&args.to, new_dst.join("\n") + "\n")
        .unwrap_or_else(|e| fail(&format!("write {}: {e}", args.to)));
    println!("\napplied.");
}
