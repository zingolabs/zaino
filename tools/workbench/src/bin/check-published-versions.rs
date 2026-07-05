//! Guard: no publishable crate reuses an already-published version number
//! with different content.
//!
//! For every crate `cargo package --workspace` produces, look its exact
//! version up on the crates.io sparse index. A version that is not on the
//! index is fine (a fresh number, or a never-published crate). A published
//! version is fine only when the packaged content is byte-identical to the
//! published `.crate` — an unchanged crate legitimately keeps its number and
//! is simply skipped at release. Anything else is a version-reuse violation:
//! the tree cannot be released until that crate's version is bumped.
//!
//! `.cargo_vcs_info.json` is excluded from the comparison — it embeds the
//! packaging commit's SHA, so it differs on every commit even when the
//! source is identical. Yanked versions count as published: crates.io never
//! frees a version number.
//!
//! `--mode advisory` reports violations as warnings and exits 0 (feature
//! branches, where unbumped-but-changed crates are the normal
//! bump-at-release state). `--mode blocking` reports them as errors and
//! exits 1 (`rc/**` and `stable` release gates).
//!
//! Std-only by crate design: network, extraction, and diffing go through
//! `curl`, `tar`, and `diff` subprocesses.

use std::path::{Path, PathBuf};
use std::process::Command;
use workbench::{repo_root, run};

const PROG: &str = "check-published-versions";
const DIFF_LINE_CAP: usize = 120;

/// Failure disposition for version-reuse violations. Infrastructure errors
/// (network, subprocess) fail loudly in either mode — a check that could not
/// run must not read as a pass.
enum Mode {
    Advisory,
    Blocking,
}

struct PackagedCrate {
    name: String,
    version: String,
}

impl PackagedCrate {
    fn id(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

fn main() {
    run(PROG, check, |summary| println!("{PROG}: {summary}"))
}

fn check() -> Result<String, Vec<String>> {
    let mode = mode_from_args(std::env::args().skip(1))?;
    let root = repo_root()?;
    let scratch = create_scratch_dir()?;

    let packaged = package_workspace(&root)?;
    let mut violations = Vec::new();
    for krate in &packaged {
        if let Comparison::Differs(diff) = compare_against_published(&root, &scratch, krate)? {
            violations.push((krate, diff));
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    write_github_output(!violations.is_empty())?;

    if violations.is_empty() {
        return Ok(format!(
            "ok — none of the {} publishable crates reuses a published version with different content",
            packaged.len()
        ));
    }

    let offenders: Vec<String> = violations.iter().map(|(k, _)| k.id()).collect();
    let summary = format!(
        "version-reuse violation(s): {} — already published with different content; \
         bump these versions before the next release",
        offenders.join(", ")
    );
    match mode {
        Mode::Advisory => {
            for (krate, diff) in &violations {
                println!(
                    "{PROG}: {} is published with different content:",
                    krate.id()
                );
                println!("{diff}");
            }
            println!("::warning::{summary}");
            Ok(summary)
        }
        Mode::Blocking => {
            let mut lines = vec![format!("::error::{summary}")];
            for (krate, diff) in violations {
                lines.push(format!(
                    "{} is published with different content:",
                    krate.id()
                ));
                lines.push(diff);
            }
            Err(lines)
        }
    }
}

/// Publish a `violations=<bool>` step output when running under GitHub
/// Actions (the `GITHUB_OUTPUT` file is set). Downstream steps use it to
/// degrade the publish dry-run to `--no-verify`: a crate that reuses a
/// published version is *shadowed* by the registry during verify builds
/// (cargo resolves the already-published artifact over the workspace
/// overlay), so packaged-form verification of its dependents is meaningless
/// until versions are bumped. Outside GitHub Actions this is a no-op.
fn write_github_output(violations: bool) -> Result<(), Vec<String>> {
    let Ok(path) = std::env::var("GITHUB_OUTPUT") else {
        return Ok(());
    };
    let line = format!("violations={violations}\n");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()))
        .map_err(|e| vec![format!("cannot append to GITHUB_OUTPUT ({path}): {e}")])
}

/// A per-process scratch directory for downloads and extractions, removed
/// (best-effort) at the end of the check.
fn create_scratch_dir() -> Result<PathBuf, Vec<String>> {
    let dir = std::env::temp_dir().join(format!("{PROG}-{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| vec![format!("cannot create scratch dir {}: {e}", dir.display())])?;
    Ok(dir)
}

fn mode_from_args(mut args: impl Iterator<Item = String>) -> Result<Mode, Vec<String>> {
    match (args.next().as_deref(), args.next().as_deref(), args.next()) {
        (Some("--mode"), Some("advisory"), None) => Ok(Mode::Advisory),
        (Some("--mode"), Some("blocking"), None) => Ok(Mode::Blocking),
        _ => Err(vec![format!("usage: {PROG} --mode <advisory|blocking>")]),
    }
}

/// Package every publishable workspace member (their `.crate` files land in
/// `<root>/target/package/`) and return the packaged name/version pairs.
///
/// This goes through `cargo publish --dry-run` rather than `cargo package`:
/// only the former skips `publish = false` members (`cargo package
/// --workspace` fails on the live-test crates' versionless path
/// dependencies). `--workspace` resolves sibling dependencies against a
/// local overlay of the registry, so this succeeds even when members depend
/// on unpublished sibling changes. `--allow-dirty` keeps local
/// (uncommitted-tree) runs useful; CI checkouts are clean regardless.
fn package_workspace(root: &Path) -> Result<Vec<PackagedCrate>, Vec<String>> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args([
            "publish",
            "--workspace",
            "--dry-run",
            "--no-verify",
            "--allow-dirty",
        ])
        .output()
        .map_err(|e| vec![format!("failed to run cargo publish --dry-run: {e}")])?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let mut lines =
            vec!["`cargo publish --workspace --dry-run --no-verify` failed:".to_string()];
        lines.extend(stderr.lines().map(str::to_string));
        return Err(lines);
    }
    let packaged: Vec<PackagedCrate> = stderr.lines().filter_map(packaged_crate_line).collect();
    if packaged.is_empty() {
        return Err(vec![
            "cargo package reported no packaged crates — nothing to check".to_string(),
        ]);
    }
    Ok(packaged)
}

/// Parse a cargo status line `"   Packaging zaino-common v0.2.0 (/path)"`.
fn packaged_crate_line(line: &str) -> Option<PackagedCrate> {
    let rest = line.trim_start().strip_prefix("Packaging ")?;
    let mut words = rest.split_whitespace();
    let name = words.next()?.to_string();
    let version = words.next()?.strip_prefix('v')?.to_string();
    Some(PackagedCrate { name, version })
}

enum Comparison {
    /// The exact version is not on the index (or the crate never published).
    NotPublished,
    /// Published, and the packaged content is byte-identical.
    Identical,
    /// Published with different content — the violation. Carries the
    /// (possibly truncated) unified diff, published → local.
    Differs(String),
}

fn compare_against_published(
    root: &Path,
    scratch: &Path,
    krate: &PackagedCrate,
) -> Result<Comparison, Vec<String>> {
    let index_body = match fetch_index(scratch, &krate.name)? {
        IndexLookup::NeverPublished => return Ok(Comparison::NotPublished),
        IndexLookup::Versions(body) => body,
    };
    if !index_lists_version(&index_body, &krate.version) {
        return Ok(Comparison::NotPublished);
    }

    let published_crate = download_published(scratch, krate)?;
    let local_crate = local_crate_path(root, krate)?;

    let published_dir = scratch.join(krate.id()).join("published");
    let local_dir = scratch.join(krate.id()).join("local");
    extract(&published_crate, &published_dir)?;
    extract(&local_crate, &local_dir)?;

    diff_trees(&published_dir, &local_dir)
}

/// The `.crate` file [`package_workspace`] produced for `krate`. A
/// single-package `cargo package` writes to `target/package/`; the
/// multi-package `cargo publish --workspace --dry-run` stages its output in
/// `target/package/tmp-crate/` instead, so both locations are probed.
fn local_crate_path(root: &Path, krate: &PackagedCrate) -> Result<PathBuf, Vec<String>> {
    let file = format!("{}-{}.crate", krate.name, krate.version);
    let candidates = [
        root.join("target/package").join(&file),
        root.join("target/package/tmp-crate").join(&file),
    ];
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            vec![format!(
                "packaged {} not found at {} or {}",
                krate.id(),
                candidates[0].display(),
                candidates[1].display()
            )]
        })
}

enum IndexLookup {
    NeverPublished,
    Versions(String),
}

fn fetch_index(scratch: &Path, name: &str) -> Result<IndexLookup, Vec<String>> {
    let body_file = scratch.join(format!("{name}.index"));
    let url = format!("https://index.crates.io/{}", index_path(name));
    let status_code = curl_with_status(&url, &body_file)?;
    match status_code.as_str() {
        "200" => Ok(IndexLookup::Versions(workbench::read(&body_file)?)),
        "404" => Ok(IndexLookup::NeverPublished),
        other => Err(vec![format!(
            "sparse-index lookup for {name} returned HTTP {other} ({url})"
        )]),
    }
}

/// Download `url` to `out`, returning the HTTP status code. Transport-level
/// failures (DNS, TLS, timeouts) are errors; HTTP status is the caller's to
/// interpret.
fn curl_with_status(url: &str, out: &Path) -> Result<String, Vec<String>> {
    let output = Command::new("curl")
        .args(["--silent", "--show-error", "--retry", "3"])
        .args(["--output".as_ref(), out.as_os_str()])
        .args(["--write-out", "%{http_code}", url])
        .output()
        .map_err(|e| vec![format!("failed to run curl: {e}")])?;
    if !output.status.success() {
        return Err(vec![format!(
            "curl {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )]);
    }
    String::from_utf8(output.stdout).map_err(|e| vec![format!("curl status not utf-8: {e}")])
}

/// crates.io sparse-index path for a crate name (the registry's length rules).
fn index_path(name: &str) -> String {
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

/// Whether the sparse-index body lists `version`. Yanked entries still count:
/// a yanked version number can never be republished.
fn index_lists_version(index_body: &str, version: &str) -> bool {
    index_body
        .lines()
        .any(|line| vers_value(line).is_some_and(|v| v == version))
}

/// The `"vers"` value of one sparse-index JSON line. Only the top-level
/// version record carries a `"vers":"…"` string pair, so a substring scan is
/// sufficient for the machine-generated index format.
fn vers_value(line: &str) -> Option<&str> {
    let start = line.find("\"vers\":\"")? + "\"vers\":\"".len();
    let rest = &line[start..];
    Some(&rest[..rest.find('"')?])
}

fn download_published(scratch: &Path, krate: &PackagedCrate) -> Result<PathBuf, Vec<String>> {
    let out = scratch.join(format!("{}.published.crate", krate.id()));
    let url = format!(
        "https://static.crates.io/crates/{}/{}-{}.crate",
        krate.name, krate.name, krate.version
    );
    let status_code = curl_with_status(&url, &out)?;
    if status_code != "200" {
        return Err(vec![format!(
            "download of published {} returned HTTP {status_code} ({url})",
            krate.id()
        )]);
    }
    Ok(out)
}

fn extract(archive: &Path, dest: &Path) -> Result<(), Vec<String>> {
    std::fs::create_dir_all(dest)
        .map_err(|e| vec![format!("cannot create {}: {e}", dest.display())])?;
    let output = Command::new("tar")
        .args(["--extract", "--gzip", "--file"])
        .arg(archive)
        .args(["--directory".as_ref(), dest.as_os_str()])
        .output()
        .map_err(|e| vec![format!("failed to run tar: {e}")])?;
    if !output.status.success() {
        return Err(vec![format!(
            "tar extraction of {} failed: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )]);
    }
    Ok(())
}

/// Recursive unified diff, published → local, excluding the always-different
/// `.cargo_vcs_info.json` (it embeds the packaging commit's SHA).
fn diff_trees(published_dir: &Path, local_dir: &Path) -> Result<Comparison, Vec<String>> {
    let output = Command::new("diff")
        .args([
            "--recursive",
            "--unified",
            "--exclude",
            ".cargo_vcs_info.json",
        ])
        .args([published_dir.as_os_str(), local_dir.as_os_str()])
        .output()
        .map_err(|e| vec![format!("failed to run diff: {e}")])?;
    match output.status.code() {
        Some(0) => Ok(Comparison::Identical),
        Some(1) => Ok(Comparison::Differs(truncate_lines(
            &String::from_utf8_lossy(&output.stdout),
            DIFF_LINE_CAP,
        ))),
        _ => Err(vec![format!(
            "diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )]),
    }
}

/// First `cap` lines of `text`, with an elision note when truncated.
fn truncate_lines(text: &str, cap: usize) -> String {
    let total = text.lines().count();
    if total <= cap {
        return text.trim_end().to_string();
    }
    let kept: Vec<&str> = text.lines().take(cap).collect();
    format!(
        "{}\n… (diff truncated: {} more lines; run `cargo run --manifest-path \
         tools/workbench/Cargo.toml --bin {PROG} -- --mode advisory` locally for the full diff)",
        kept.join("\n"),
        total - cap
    )
}

#[cfg(test)]
mod packaged_crate_line {
    use super::packaged_crate_line;

    #[test]
    fn parses_cargo_status_lines() {
        let parsed = packaged_crate_line("   Packaging zaino-common v0.2.0 (/w/zaino-common)")
            .expect("status line parses");
        assert_eq!(parsed.name, "zaino-common");
        assert_eq!(parsed.version, "0.2.0");
    }

    #[test]
    fn keeps_prerelease_versions_intact() {
        let parsed = packaged_crate_line("   Packaging zainod v0.4.3-ironwood.1 (/w/zainod)")
            .expect("prerelease line parses");
        assert_eq!(parsed.version, "0.4.3-ironwood.1");
    }

    #[test]
    fn ignores_other_cargo_output() {
        assert!(packaged_crate_line("    Updating crates.io index").is_none());
        assert!(packaged_crate_line("warning: crate zaino-proto@0.1.3 already exists").is_none());
        assert!(packaged_crate_line("   Packaged 17 files, 135.4KiB").is_none());
    }
}

#[cfg(test)]
mod index_path {
    use super::index_path;

    #[test]
    fn follows_the_registry_length_rules() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("zainod"), "za/in/zainod");
        assert_eq!(index_path("zaino-state"), "za/in/zaino-state");
    }
}

#[cfg(test)]
mod index_lists_version {
    use super::index_lists_version;

    const INDEX: &str = concat!(
        r#"{"name":"zainod","vers":"0.3.0","deps":[{"name":"vers","req":"^1"}],"yanked":false}"#,
        "\n",
        r#"{"name":"zainod","vers":"0.3.1","deps":[],"yanked":true}"#,
        "\n",
    );

    #[test]
    fn finds_exact_versions_only() {
        assert!(index_lists_version(INDEX, "0.3.0"));
        assert!(!index_lists_version(INDEX, "0.3"));
        assert!(!index_lists_version(INDEX, "0.3.2"));
    }

    #[test]
    fn yanked_versions_still_count_as_published() {
        assert!(index_lists_version(INDEX, "0.3.1"));
    }
}

#[cfg(test)]
mod truncate_lines {
    use super::truncate_lines;

    #[test]
    fn short_diffs_pass_through() {
        assert_eq!(truncate_lines("a\nb\n", 3), "a\nb");
    }

    #[test]
    fn long_diffs_are_capped_with_an_elision_note() {
        let capped = truncate_lines("a\nb\nc\nd\n", 2);
        assert!(capped.starts_with("a\nb\n…"));
        assert!(capped.contains("2 more lines"));
    }
}
