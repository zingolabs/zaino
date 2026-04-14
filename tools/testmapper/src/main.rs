use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};
use rusqlite::Connection;

fn repo_root() -> PathBuf {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("failed to run git rev-parse --show-toplevel");
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

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
    /// Show which lines a test exercised
    Show {
        /// Exact test name
        test: String,
        /// Filter to a specific file path (substring match)
        #[arg(long)]
        file: Option<String>,
    },
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

fn migrate_db(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    match version {
        0 => {
            conn.execute_batch(
                "CREATE TABLE runs (
                    id INTEGER PRIMARY KEY,
                    test_name TEXT NOT NULL,
                    commit_hash TEXT NOT NULL,
                    zainod_version TEXT NOT NULL,
                    zcashd_version TEXT NOT NULL,
                    zebrad_version TEXT NOT NULL,
                    timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                    raw_json BLOB,
                    UNIQUE(test_name, commit_hash, zainod_version, zcashd_version, zebrad_version)
                );

                CREATE TABLE covered_lines (
                    run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                    file_path TEXT NOT NULL,
                    line_number INTEGER NOT NULL,
                    hit_count INTEGER NOT NULL,
                    PRIMARY KEY (run_id, file_path, line_number)
                );

                CREATE INDEX idx_covered_lines_file
                    ON covered_lines(file_path, line_number);

                PRAGMA user_version = 1;",
            )?;
        }
        1 => {}
        v => panic!("unknown schema version {v} — is this DB from a newer testmapper?"),
    }
    Ok(())
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

fn parse_env_file(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn get_zainod_version(root: &Path) -> String {
    std::fs::read_to_string(root.join("Cargo.toml"))
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            let line = line.trim();
            if line.starts_with("version") && line.contains('=') {
                let (_, v) = line.split_once('=')?;
                Some(v.trim().trim_matches('"').to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

struct Versions {
    zainod: String,
    zcashd: String,
    zebrad: String,
}

fn get_versions(root: &Path) -> Versions {
    let env = parse_env_file(&root.join(".env.testing-artifacts"));
    Versions {
        zainod: get_zainod_version(root),
        zcashd: env.get("ZCASH_VERSION").cloned().unwrap_or_else(|| "unknown".to_string()),
        zebrad: env.get("ZEBRA_VERSION").cloned().unwrap_or_else(|| "unknown".to_string()),
    }
}

fn run_exists(conn: &Connection, test_name: &str, commit: &str, versions: &Versions) -> bool {
    conn.query_row(
        "SELECT 1 FROM runs WHERE test_name = ?1 AND commit_hash = ?2 \
         AND zainod_version = ?3 AND zcashd_version = ?4 AND zebrad_version = ?5",
        rusqlite::params![test_name, commit, versions.zainod, versions.zcashd, versions.zebrad],
        |_| Ok(()),
    )
    .is_ok()
}

/// Returns (test_name, binary_name) pairs from `cargo nextest list`.
/// Binary names are the `--test` argument for scoped builds.
fn list_tests(root: &Path) -> Vec<(String, String)> {
    let manifest = root.join("integration-tests/Cargo.toml");
    let output = Command::new("cargo")
        .args([
            "nextest",
            "list",
            "--manifest-path",
            &manifest.to_string_lossy(),
            "--message-format",
            "human",
        ])
        .stderr(Stdio::inherit())
        .output()
        .expect("failed to run cargo nextest list");

    let mut results = Vec::new();
    let mut current_binary = String::new();

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if !line.starts_with(' ') && line.contains("::") && line.ends_with(':') {
            // Binary header like "integration-tests::test_vectors:"
            // Extract the part after the last "::" before the trailing ":"
            let trimmed = line.trim_end_matches(':');
            current_binary = trimmed
                .rsplit("::")
                .next()
                .unwrap_or(trimmed)
                .to_string();
        } else if line.starts_with("    ") && !current_binary.is_empty() {
            let test_name = line.trim().trim_end_matches(':').to_string();
            if !test_name.is_empty() {
                results.push((test_name, current_binary.clone()));
            }
        }
    }

    results
}

fn resolve_test_binary(test_name: &str, root: &Path) -> Option<String> {
    // Fast path: grep source files for the test function name
    let tests_dir = root.join("integration-tests/tests/");
    let output = Command::new("grep")
        .args(["-rl", &format!("fn {test_name}"), &tests_dir.to_string_lossy()])
        .output()
        .ok();

    if let Some(output) = output {
        if let Some(binary) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|path| {
                std::path::Path::new(path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
            })
        {
            return Some(binary);
        }
    }

    // Slow fallback: ask cargo nextest (triggers a build)
    eprintln!("grep lookup failed for {test_name}, falling back to cargo nextest list...");
    list_tests(root)
        .into_iter()
        .find(|(name, _)| name == test_name)
        .map(|(_, binary)| binary)
}

fn collect_coverage(test_name: &str, binary: &str, root: &Path) -> Option<serde_json::Value> {
    let manifest = root.join("integration-tests/Cargo.toml");
    let output = Command::new("cargo")
        .args([
            "llvm-cov",
            "nextest",
            "--manifest-path",
            &manifest.to_string_lossy(),
            "--test",
            binary,
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
    versions: &Versions,
    json: &serde_json::Value,
) -> rusqlite::Result<()> {
    let raw = serde_json::to_vec(json).unwrap_or_default();

    conn.execute(
        "INSERT OR REPLACE INTO runs \
         (test_name, commit_hash, zainod_version, zcashd_version, zebrad_version, raw_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            test_name,
            commit,
            versions.zainod,
            versions.zcashd,
            versions.zebrad,
            raw,
        ],
    )?;

    let run_id: i64 = conn.query_row(
        "SELECT id FROM runs WHERE test_name = ?1 AND commit_hash = ?2 \
         AND zainod_version = ?3 AND zcashd_version = ?4 AND zebrad_version = ?5",
        rusqlite::params![test_name, commit, versions.zainod, versions.zcashd, versions.zebrad],
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
    migrate_db(&conn).expect("failed to initialize database");

    let root = repo_root();

    match cli.command {
        Cmd::Collect { test } => {
            let commit = get_commit_hash();
            let versions = get_versions(&root);

            if run_exists(&conn, &test, &commit, &versions) {
                eprintln!(
                    "Skipping {test}: already collected at {commit} \
                     (zainod={}, zcashd={}, zebrad={})",
                    versions.zainod, versions.zcashd, versions.zebrad,
                );
                return;
            }

            let binary = resolve_test_binary(&test, &root).unwrap_or_else(|| {
                eprintln!("Could not find binary for test: {test}");
                std::process::exit(1);
            });

            eprintln!(
                "Collecting coverage for: {test} (binary={binary}, zainod={}, zcashd={}, zebrad={})",
                versions.zainod, versions.zcashd, versions.zebrad,
            );
            if let Some(json) = collect_coverage(&test, &binary, &root) {
                store_coverage(&conn, &test, &commit, &versions, &json)
                    .expect("failed to store coverage");
                eprintln!("Done. Database: {db_path}");
            }
        }
        Cmd::ListTests => {
            for (test, binary) in list_tests(&root) {
                println!("{binary}\t{test}");
            }
        }
        Cmd::Show { test, file } => {
            let commit = get_commit_hash();
            let versions = get_versions(&root);

            let run_id: Option<i64> = conn
                .query_row(
                    "SELECT id FROM runs WHERE test_name = ?1 AND commit_hash = ?2 \
                     AND zainod_version = ?3 AND zcashd_version = ?4 AND zebrad_version = ?5",
                    rusqlite::params![
                        test, commit, versions.zainod, versions.zcashd, versions.zebrad,
                    ],
                    |row| row.get(0),
                )
                .ok();

            let Some(run_id) = run_id else {
                eprintln!("No coverage data for {test} at {commit}");
                eprintln!(
                    "Run: makers collect-test-coverage {test}",
                );
                std::process::exit(1);
            };

            let pattern = file.as_deref().map(|f| format!("%{f}%"));
            let pattern_ref = pattern.as_deref().unwrap_or("%");

            let mut stmt = conn
                .prepare(
                    "SELECT file_path, line_number, hit_count FROM covered_lines \
                     WHERE run_id = ?1 AND file_path LIKE ?2 \
                     ORDER BY file_path, line_number",
                )
                .expect("failed to prepare query");

            let rows: Vec<(String, i64, i64)> = stmt
                .query_map(rusqlite::params![run_id, pattern_ref], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .expect("query failed")
                .filter_map(|r| r.ok())
                .collect();

            if rows.is_empty() {
                eprintln!("No covered lines found.");
                return;
            }

            let mut current_file = String::new();
            for (path, line, count) in &rows {
                if *path != current_file {
                    if !current_file.is_empty() {
                        println!();
                    }
                    println!("{path}:");
                    current_file.clone_from(path);
                }
                println!("  {line:>6}  (x{count})");
            }

            eprintln!("\n{} lines across {} files", rows.len(), {
                let mut files: Vec<&str> = rows.iter().map(|(f, _, _)| f.as_str()).collect();
                files.dedup();
                files.len()
            });
        }
    }
}
