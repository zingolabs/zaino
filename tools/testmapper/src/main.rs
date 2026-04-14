use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};
use rusqlite::Connection;

#[derive(Parser)]
#[command(name = "testmapper", about = "Per-test coverage collection for Zaino")]
struct Cli {
    /// Path to the SQLite database
    #[arg(long, default_value = "~/.cache/zaino/coverage.db")]
    db: String,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Collect coverage for a single test
    Collect {
        /// Exact test name
        test: String,
    },
    /// List all integration tests
    ListTests,
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS runs (
            id INTEGER PRIMARY KEY,
            test_name TEXT NOT NULL,
            commit_hash TEXT NOT NULL,
            timestamp TEXT NOT NULL DEFAULT (datetime('now')),
            raw_json BLOB,
            UNIQUE(test_name, commit_hash)
        );

        CREATE TABLE IF NOT EXISTS covered_lines (
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            file_path TEXT NOT NULL,
            line_number INTEGER NOT NULL,
            hit_count INTEGER NOT NULL,
            PRIMARY KEY (run_id, file_path, line_number)
        );

        CREATE INDEX IF NOT EXISTS idx_covered_lines_file
            ON covered_lines(file_path, line_number);",
    )
}

fn get_commit_hash() -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn list_tests() -> Vec<String> {
    let output = Command::new("cargo")
        .args([
            "nextest",
            "list",
            "--manifest-path",
            "integration-tests/Cargo.toml",
            "--message-format",
            "human",
        ])
        .stderr(Stdio::inherit())
        .output()
        .expect("failed to run cargo nextest list");

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("    ") && line.contains("::"))
        .map(|line| line.trim().trim_end_matches(':').to_string())
        .collect()
}

fn collect_coverage(test_name: &str) -> Option<serde_json::Value> {
    let output = Command::new("cargo")
        .args([
            "llvm-cov",
            "nextest",
            "--manifest-path",
            "integration-tests/Cargo.toml",
            "-E",
            &format!("test(={test_name})"),
            "--json",
        ])
        .stderr(Stdio::inherit())
        .output()
        .expect("failed to run cargo llvm-cov");

    if !output.status.success() {
        eprintln!("coverage collection failed for {test_name}");
        return None;
    }

    serde_json::from_slice(&output.stdout).ok()
}

fn store_coverage(
    conn: &Connection,
    test_name: &str,
    commit: &str,
    json: &serde_json::Value,
) -> rusqlite::Result<()> {
    let raw = serde_json::to_vec(json).unwrap_or_default();

    conn.execute(
        "INSERT OR REPLACE INTO runs (test_name, commit_hash, raw_json) VALUES (?1, ?2, ?3)",
        rusqlite::params![test_name, commit, raw],
    )?;

    let run_id: i64 = conn.query_row(
        "SELECT id FROM runs WHERE test_name = ?1 AND commit_hash = ?2",
        rusqlite::params![test_name, commit],
        |row| row.get(0),
    )?;

    conn.execute("DELETE FROM covered_lines WHERE run_id = ?1", [run_id])?;

    if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
        for entry in data {
            if let Some(files) = entry.get("files").and_then(|f| f.as_array()) {
                for file in files {
                    let path = file
                        .get("filename")
                        .and_then(|f| f.as_str())
                        .unwrap_or("");

                    if !path.contains("packages/") {
                        continue;
                    }

                    if let Some(segments) = file.get("segments").and_then(|s| s.as_array()) {
                        for seg in segments {
                            if let Some(arr) = seg.as_array() {
                                if arr.len() >= 3 {
                                    let line = arr[0].as_i64().unwrap_or(0);
                                    let count = arr[2].as_i64().unwrap_or(0);
                                    if line > 0 {
                                        conn.execute(
                                            "INSERT OR REPLACE INTO covered_lines \
                                             (run_id, file_path, line_number, hit_count) \
                                             VALUES (?1, ?2, ?3, ?4)",
                                            rusqlite::params![run_id, path, line, count],
                                        )?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn main() {
    let cli = Cli::parse();

    let db_path = expand_tilde(&cli.db);
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let conn = Connection::open(&db_path).expect("failed to open database");
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .expect("failed to set pragmas");
    init_db(&conn).expect("failed to initialize database");

    match cli.command {
        Cmd::Collect { test } => {
            let commit = get_commit_hash();
            eprintln!("Collecting coverage for: {test}");
            if let Some(json) = collect_coverage(&test) {
                store_coverage(&conn, &test, &commit, &json)
                    .expect("failed to store coverage");
                eprintln!("Done. Database: {db_path}");
            }
        }
        Cmd::ListTests => {
            for test in list_tests() {
                println!("{test}");
            }
        }
    }
}
