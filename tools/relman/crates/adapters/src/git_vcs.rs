use std::path::PathBuf;
use std::process::Command;

use relman_core::ports::{Vcs, VcsError};

/// A [`Vcs`] over a real `git` working tree.
///
/// Computes a PR's changed files as the three-dot diff
/// `git diff --name-only <base>...HEAD` — the changes on `HEAD` since its
/// merge-base with `base` — so an out-of-date base branch does not surface as
/// spurious changes. Runs `git` in `dir` (the repo root); output paths are
/// repo-relative.
pub struct GitVcs {
    dir: PathBuf,
}

impl GitVcs {
    /// Root the adapter at `dir`, the working tree `git` runs in.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Vcs for GitVcs {
    fn changed_files(&self, base: &str) -> Result<Vec<PathBuf>, VcsError> {
        let range = format!("{base}...HEAD");
        let output = Command::new("git")
            .current_dir(&self.dir)
            .args(["diff", "--name-only", &range])
            .output()
            .map_err(VcsError::Spawn)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(VcsError::Command {
                command: format!("git diff --name-only {range}"),
                stderr,
            });
        }

        let stdout = String::from_utf8(output.stdout).map_err(VcsError::Encoding)?;
        Ok(stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Run `git <args>` in `dir` with a throwaway identity and no ambient
    /// (global/system) config, so the test never touches the developer's git
    /// setup and never blocks on GPG signing.
    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "relman-test")
            .env("GIT_AUTHOR_EMAIL", "relman-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "relman-test")
            .env("GIT_COMMITTER_EMAIL", "relman-test@example.invalid")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn three_dot_diff_reports_exactly_the_branch_changes() {
        let repo = tempfile::tempdir().expect("temp dir");
        let root = repo.path();

        // Base commit: one file, on a named `base` ref.
        git(root, &["init", "-q"]);
        std::fs::write(root.join("base.txt"), "base\n").expect("write base");
        git(root, &["add", "base.txt"]);
        git(
            root,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        );
        git(root, &["branch", "base"]);

        // Feature branch: modify base.txt and add new.txt.
        git(root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("base.txt"), "base changed\n").expect("modify base");
        std::fs::write(root.join("new.txt"), "new\n").expect("write new");
        git(root, &["add", "base.txt", "new.txt"]);
        git(
            root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "feature work",
            ],
        );

        let vcs = GitVcs::new(root.to_path_buf());
        let mut changed: Vec<String> = vcs
            .changed_files("base")
            .expect("diff runs")
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        changed.sort();
        assert_eq!(changed, ["base.txt", "new.txt"]);
    }
}
