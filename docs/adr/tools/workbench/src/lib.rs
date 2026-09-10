#![forbid(unsafe_code)]

use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Marker line that opens the generated index block in the root README.
pub const INDEX_BEGIN: &str = "<!-- ledger-index:begin -->";
/// Marker line that closes the generated index block in the root README.
pub const INDEX_END: &str = "<!-- ledger-index:end -->";

/// Directory names at the ledger root that never hold records.
const NON_SCOPE_DIRS: &[&str] = &["tools", "target"];

/// Every rule violation found in one pass over the ledger.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Violations(pub Vec<String>);

impl fmt::Display for Violations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in &self.0 {
            writeln!(f, "{line}")?;
        }
        Ok(())
    }
}

/// A record's standing, drawn from the closed status vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Proposed,
    Accepted,
    /// Holds the successor's path relative to the ledger root.
    Superseded(PathBuf),
}

/// One record file as the checker understands it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Scope directory relative to the root, empty for the org scope.
    pub scope: String,
    pub number_text: String,
    pub number: u32,
    pub file_name: String,
    pub title: String,
    pub status: Status,
}

impl Record {
    /// Path relative to the ledger root.
    pub fn path(&self) -> PathBuf {
        scope_dir(&self.scope).join(&self.file_name)
    }

    /// Citation form seen from the ledger root, such as `003` or `zaino/0016`.
    pub fn qualified_name(&self) -> String {
        qualify(&self.scope, &self.number_text)
    }
}

/// Parse and validate every record under `root`, returning them in index order.
pub fn check(root: &Path) -> Result<Vec<Record>, Violations> {
    let mut violations = Vec::new();
    let mut records = Vec::new();
    for scope in scopes(root).map_err(|e| Violations(vec![e]))? {
        let dir = root.join(scope_dir(&scope));
        let mut scope_records = Vec::new();
        for entry in read_dir_sorted(&dir).map_err(|e| Violations(vec![e]))? {
            let file_name = base_name(&entry);
            if entry.is_dir() || file_name == "README.md" || !file_name.ends_with(".md") {
                continue;
            }
            match parse_record(root, &scope, &file_name) {
                Ok(record) => scope_records.push(record),
                Err(problem) => violations.push(problem),
            }
        }
        check_unique_numbers(&scope_records, &mut violations);
        records.extend(scope_records);
    }
    for record in &records {
        if let Status::Superseded(successor) = &record.status {
            if !records.iter().any(|r| &r.path() == successor) {
                violations.push(format!(
                    "{}: `superseded by` target {} is not a record",
                    record.path().display(),
                    successor.display()
                ));
            }
        }
    }
    if violations.is_empty() {
        Ok(records)
    } else {
        Err(Violations(violations))
    }
}

/// Render the per-scope index tables for `records`.
pub fn render_index(records: &[Record]) -> String {
    let mut out = String::new();
    let mut scope_names: Vec<&str> = records.iter().map(|r| r.scope.as_str()).collect();
    scope_names.sort_unstable();
    scope_names.dedup();
    for scope in scope_names {
        let heading = if scope.is_empty() {
            "### Org-scoped records".to_string()
        } else {
            format!("### Repo-scoped records: {scope}")
        };
        out.push_str(&format!(
            "{heading}\n\n| Number | Record | Status |\n| --- | --- | --- |\n"
        ));
        for record in records.iter().filter(|r| r.scope == scope) {
            out.push_str(&format!(
                "| {} | [{}]({}) | {} |\n",
                record.number_text,
                record.title,
                record.path().display(),
                render_status(records, &record.status)
            ));
        }
        out.push('\n');
    }
    out
}

/// Return `readme` with the block between the index markers replaced by `index`.
pub fn readme_with_index(readme: &str, index: &str) -> Result<String, String> {
    let begin = readme
        .find(INDEX_BEGIN)
        .ok_or_else(|| format!("README.md lacks the `{INDEX_BEGIN}` marker"))?;
    let after_begin = begin + INDEX_BEGIN.len();
    let end = readme[after_begin..]
        .find(INDEX_END)
        .map(|offset| after_begin + offset)
        .ok_or_else(|| {
            format!("README.md lacks the `{INDEX_END}` marker after the begin marker")
        })?;
    Ok(format!(
        "{}\n{}{}",
        &readme[..after_begin],
        index,
        &readme[end..]
    ))
}

/// Compare or rewrite the README's index block, returning whether it was already current.
pub fn sync_readme(root: &Path, records: &[Record], write: bool) -> Result<bool, String> {
    let readme_path = root.join("README.md");
    let readme =
        fs::read_to_string(&readme_path).map_err(|e| format!("{}: {e}", readme_path.display()))?;
    let expected = readme_with_index(&readme, &render_index(records))?;
    if expected == readme {
        return Ok(true);
    }
    if write {
        fs::write(&readme_path, expected).map_err(|e| format!("{}: {e}", readme_path.display()))?;
    }
    Ok(false)
}

/// Scope names in index order: the org scope first, then every record subdirectory.
fn scopes(root: &Path) -> Result<Vec<String>, String> {
    let mut names = vec![String::new()];
    for entry in read_dir_sorted(root)? {
        let name = base_name(&entry);
        if !entry.is_dir() || name.starts_with('.') || NON_SCOPE_DIRS.contains(&name.as_str()) {
            continue;
        }
        let holds_markdown = read_dir_sorted(&entry)?
            .iter()
            .any(|p| p.extension().is_some_and(|ext| ext == "md"));
        if holds_markdown {
            names.push(name);
        }
    }
    Ok(names)
}

/// The final path component as text, empty when the path has none.
fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn scope_dir(scope: &str) -> PathBuf {
    if scope.is_empty() {
        PathBuf::new()
    } else {
        PathBuf::from(scope)
    }
}

fn qualify(scope: &str, number_text: &str) -> String {
    if scope.is_empty() {
        number_text.to_string()
    } else {
        format!("{scope}/{number_text}")
    }
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    paths.sort();
    Ok(paths)
}

fn parse_record(root: &Path, scope: &str, file_name: &str) -> Result<Record, String> {
    let relative = scope_dir(scope).join(file_name);
    let label = relative.display().to_string();
    let (number_text, number) = parse_file_name(file_name).ok_or_else(|| {
        format!("{label}: file name is not `NNN-kebab-title.md` (or `NNNN-` in a repo scope)")
    })?;
    let text = fs::read_to_string(root.join(&relative)).map_err(|e| format!("{label}: {e}"))?;
    let title = text
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .ok_or_else(|| format!("{label}: no `# ` title line"))?
        .trim()
        .to_string();
    let status_line = status_line(&text)
        .ok_or_else(|| format!("{label}: no `## Status` section with a status line"))?;
    let status = parse_status(status_line, &scope_dir(scope))
        .map_err(|why| format!("{label}: status line `{status_line}` {why}"))?;
    Ok(Record {
        scope: scope.to_string(),
        number_text,
        number,
        file_name: file_name.to_string(),
        title,
        status,
    })
}

/// Split `NNN-kebab-title.md` into its number text and value.
fn parse_file_name(file_name: &str) -> Option<(String, u32)> {
    let stem = file_name.strip_suffix(".md")?;
    let (digits, title) = stem.split_once('-')?;
    let digits_ok = (3..=4).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
    let title_ok = !title.is_empty()
        && !title.starts_with('-')
        && !title.ends_with('-')
        && !title.contains("--")
        && title
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !digits_ok || !title_ok {
        return None;
    }
    Some((digits.to_string(), digits.parse().ok()?))
}

/// The first non-blank line under the `## Status` heading, if the section exists.
fn status_line(text: &str) -> Option<&str> {
    text.lines()
        .skip_while(|line| line.trim_end() != "## Status")
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .map(str::trim)
        .find(|line| !line.is_empty())
}

fn parse_status(line: &str, scope_dir: &Path) -> Result<Status, String> {
    match line {
        "proposed" => Ok(Status::Proposed),
        "accepted" => Ok(Status::Accepted),
        _ => {
            let citation = line.strip_prefix("superseded by ").ok_or_else(|| {
                "is not `proposed`, `accepted`, or `superseded by <link>`".to_string()
            })?;
            let target = link_target(citation).ok_or_else(|| {
                "must cite the successor as a Markdown link `[name](path)`".to_string()
            })?;
            if target.contains("://") {
                return Err("must link the successor by relative path, not URL".to_string());
            }
            Ok(Status::Superseded(normalize(&scope_dir.join(target))))
        }
    }
}

/// The `path` of a `[text](path)` link that ends the citation, if the citation is one.
fn link_target(citation: &str) -> Option<&str> {
    let citation = citation.trim_end_matches(['.', ' ']);
    let open = citation.rfind("](")?;
    let target = citation[open + 2..].strip_suffix(')')?;
    citation.starts_with('[').then_some(target)
}

/// Collapse `.` and `..` components without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn check_unique_numbers(records: &[Record], violations: &mut Vec<String>) {
    for (i, record) in records.iter().enumerate() {
        if let Some(twin) = records[..i].iter().find(|r| r.number == record.number) {
            violations.push(format!(
                "{}: number {} already taken by {}",
                record.path().display(),
                record.number_text,
                twin.path().display()
            ));
        }
    }
}

fn render_status(records: &[Record], status: &Status) -> String {
    match status {
        Status::Proposed => "proposed".to_string(),
        Status::Accepted => "accepted".to_string(),
        Status::Superseded(successor) => {
            let name = records
                .iter()
                .find(|r| &r.path() == successor)
                .map(Record::qualified_name)
                .unwrap_or_else(|| successor.display().to_string());
            format!("superseded by [{name}]({})", successor.display())
        }
    }
}

#[cfg(test)]
mod parse_file_name {
    use super::parse_file_name;

    #[test]
    fn accepts_three_and_four_digit_kebab_names() {
        assert_eq!(parse_file_name("003-a-b.md"), Some(("003".to_string(), 3)));
        assert_eq!(
            parse_file_name("0016-x9.md"),
            Some(("0016".to_string(), 16))
        );
    }

    #[test]
    fn rejects_spaces_uppercase_and_missing_extension() {
        assert_eq!(parse_file_name("ADR 001-No Upstream"), None);
        assert_eq!(parse_file_name("001-Upper.md"), None);
        assert_eq!(parse_file_name("001-a--b.md"), None);
        assert_eq!(parse_file_name("01-a.md"), None);
    }
}

#[cfg(test)]
mod parse_status {
    use std::path::{Path, PathBuf};

    use super::{parse_status, Status};

    #[test]
    fn resolves_a_superseded_link_relative_to_the_scope() {
        let status = parse_status("superseded by [003](../003-x.md)", Path::new("zaino"));
        assert_eq!(status, Ok(Status::Superseded(PathBuf::from("003-x.md"))));
    }

    #[test]
    fn rejects_prose_before_the_vocabulary_word() {
        assert!(parse_status("accepted (supersedes ADR-0003)", Path::new("")).is_err());
        assert!(parse_status("superseded by ADR-0016", Path::new("")).is_err());
    }
}

#[cfg(test)]
mod readme_with_index {
    use super::{readme_with_index, INDEX_BEGIN, INDEX_END};

    #[test]
    fn replaces_only_the_marked_block() {
        let readme = format!("intro\n{INDEX_BEGIN}\nold\n{INDEX_END}\noutro\n");
        let updated = readme_with_index(&readme, "new\n");
        assert_eq!(
            updated,
            Ok(format!("intro\n{INDEX_BEGIN}\nnew\n{INDEX_END}\noutro\n"))
        );
    }
}
