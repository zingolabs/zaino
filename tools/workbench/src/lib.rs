//! Shared helpers for the the workbench tooling crate (one binary per `src/bin/*.rs`).
//!
//! Every tool follows the same shape — resolve something under the repo root,
//! then either print a result or emit one-or-more `"{prog}: {line}"`
//! diagnostics and exit non-zero. [`run`] centralises that `main()` shape;
//! [`repo_root`], [`git`], and [`toolchain_channel`] are the shared primitives.

use std::path::{Path, PathBuf};
use std::process::{exit, Command};

/// Run a tool `body`, reporting diagnostics as `"{prog}: {line}"` to stderr and
/// exiting `1` on error; on success runs `on_ok` (e.g. to print a result) and
/// exits `0`. This is the single `main()` shape shared by every binary.
pub fn run<T>(
    prog: &str,
    body: impl FnOnce() -> Result<T, Vec<String>>,
    on_ok: impl FnOnce(T),
) -> ! {
    match body() {
        Ok(value) => {
            on_ok(value);
            exit(0);
        }
        Err(lines) => {
            for line in lines {
                eprintln!("{prog}: {line}");
            }
            exit(1);
        }
    }
}

/// Run `git <args>` and return its stdout, or a one-line diagnostic on failure.
pub fn git(args: &[&str]) -> Result<String, Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| vec![format!("failed to run git: {e}")])?;
    if !output.status.success() {
        return Err(vec![format!("`git {}` failed", args.join(" "))]);
    }
    String::from_utf8(output.stdout).map_err(|e| vec![format!("git output not utf-8: {e}")])
}

/// Repository root via `git rev-parse --show-toplevel`.
pub fn repo_root() -> Result<PathBuf, Vec<String>> {
    Ok(PathBuf::from(
        git(&["rev-parse", "--show-toplevel"])?.trim(),
    ))
}

/// Read `path` to a string, or a one-line `cannot read …` diagnostic.
pub fn read(path: &Path) -> Result<String, Vec<String>> {
    std::fs::read_to_string(path).map_err(|e| vec![format!("cannot read {}: {e}", path.display())])
}

/// The pinned, validated rustc channel from `<root>/rust-toolchain.toml`.
///
/// Single source of truth for `RUST_VERSION`. Rejects any non-numeric channel
/// (`stable` / `nightly` / dated pins) so the CI image tag stays reproducible.
pub fn toolchain_channel(root: &Path) -> Result<String, Vec<String>> {
    let path = root.join("rust-toolchain.toml");
    let contents = read(&path)?;

    let Some(channel) = contents.lines().find_map(channel_value) else {
        return Err(vec![format!(
            "no [toolchain].channel in {}",
            path.display()
        )]);
    };

    if !is_concrete_numeric(&channel) {
        return Err(vec![
            format!("channel '{channel}' is not a concrete numeric version (e.g. 1.95 or 1.95.0)"),
            format!(
                "a pinned rustc is required; set channel = \"<x.y[.z]>\" in {}",
                path.display()
            ),
        ]);
    }
    Ok(channel)
}

/// The zebra repository probed by `get-zebra-git-ref`.
pub const ZEBRA_REPO_URL: &str = "https://github.com/ZcashFoundation/zebra";

/// The `git ls-remote` refs to probe for a `ZEBRA_VERSION` value: its
/// `v`-prefixed release tag, a bare tag, and a branch, in that precedence.
pub fn zebra_ref_probes(version: &str) -> [String; 3] {
    [
        format!("refs/tags/v{version}"),
        format!("refs/tags/{version}"),
        format!("refs/heads/{version}"),
    ]
}

/// Resolve `ZEBRA_VERSION` to the git ref the zebra source build checks out,
/// given the `git ls-remote` output for [`zebra_ref_probes`].
///
/// `ZEBRA_VERSION` is canonically the Docker Hub image tag (bare, e.g.
/// "6.0.0-rc.0"), but zebra's git release tags carry a `v` prefix, so a
/// release version maps to `v{version}`. A branch or plain-tag pin resolves
/// as-is, and a commit SHA passes through (`ls-remote` matches ref names, not
/// commits). Anything else is an error — reported at resolve time instead of
/// surfacing as a checkout pathspec error after the container build stage has
/// cloned the whole zebra repo.
pub fn zebra_git_ref(version: &str, ls_remote_output: &str) -> Result<String, Vec<String>> {
    let v_tag = format!("refs/tags/v{version}");
    let matches_v_tag = ls_remote_output
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .any(|r| r == v_tag);

    let some_ref_matched = ls_remote_output.lines().any(|l| !l.trim().is_empty());
    if matches_v_tag {
        Ok(format!("v{version}"))
    } else if some_ref_matched || is_commit_sha_shaped(version) {
        Ok(version.to_string())
    } else {
        Err(vec![format!(
            "ZEBRA_VERSION={version} matches no zebra tag (v-prefixed or bare), \
branch, or commit-SHA shape"
        )])
    }
}

/// 7 to 40 lowercase-hex characters — an abbreviated or full git commit SHA.
fn is_commit_sha_shaped(version: &str) -> bool {
    (7..=40).contains(&version.len())
        && version
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Value of a `channel = "..."` line, mirroring `^[[:space:]]*channel[[:space:]]*=`.
/// `None` for comments, other keys, or a line without a double-quoted value.
fn channel_value(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("channel")?.trim_start();
    let value = rest.strip_prefix('=')?.trim_start().strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

/// `^[0-9]+\.[0-9]+(\.[0-9]+)?$` — two or three dot-separated all-digit parts.
fn is_concrete_numeric(channel: &str) -> bool {
    let parts: Vec<&str> = channel.split('.').collect();
    matches!(parts.len(), 2 | 3)
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_value_recognises_only_quoted_assignments() {
        assert_eq!(
            channel_value("channel = \"1.96.0\"").as_deref(),
            Some("1.96.0")
        );
        assert_eq!(channel_value("  channel=\"1.95\"").as_deref(), Some("1.95"));
        assert_eq!(channel_value("# channel = \"x\""), None);
        assert_eq!(channel_value("components = [\"clippy\"]"), None);
        assert_eq!(channel_value("[toolchain]"), None);
    }

    #[test]
    fn zebra_git_ref_prefers_the_v_tag() {
        // Annotated tags also list a peeled `^{}` line; the plain ref wins.
        let out = "abc123\trefs/tags/v6.0.0-rc.0\nabc456\trefs/tags/v6.0.0-rc.0^{}\n";
        assert_eq!(
            zebra_git_ref("6.0.0-rc.0", out).as_deref(),
            Ok("v6.0.0-rc.0")
        );
    }

    #[test]
    fn zebra_git_ref_passes_branches_and_bare_tags_through() {
        let out = "abc123\trefs/heads/main\n";
        assert_eq!(zebra_git_ref("main", out).as_deref(), Ok("main"));
    }

    #[test]
    fn zebra_git_ref_passes_sha_shapes_through_unprobed() {
        assert_eq!(
            zebra_git_ref("15d578362448fb8c4a5d29a00dcfe8adb5184082", "").as_deref(),
            Ok("15d578362448fb8c4a5d29a00dcfe8adb5184082")
        );
        assert_eq!(zebra_git_ref("15d5783", "").as_deref(), Ok("15d5783"));
    }

    #[test]
    fn zebra_git_ref_rejects_unresolvable_values() {
        assert!(zebra_git_ref("not-a-real-ref", "").is_err());
        // Short-hex-lookalike below the 7-char floor is rejected too.
        assert!(zebra_git_ref("abc", "").is_err());
    }

    #[test]
    fn numeric_validation_matches_x_y_z() {
        assert!(is_concrete_numeric("1.96.0"));
        assert!(is_concrete_numeric("1.96"));
        assert!(!is_concrete_numeric("stable"));
        assert!(!is_concrete_numeric("nightly"));
        assert!(!is_concrete_numeric("1"));
        assert!(!is_concrete_numeric("1.96.0.1"));
        assert!(!is_concrete_numeric("1..0"));
    }
}
