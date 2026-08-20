//! Pure Keep-a-Changelog rendering.
//!
//! No I/O, no clock, no config — just functions from already-derived data
//! ([`CrateBump`], parsed [`ChangeEntry`]s, a [`NaiveDate`]) to Markdown
//! strings, plus the splice that lands a rendered section into an existing
//! changelog's history. Kept apart from the service so it is unit-testable in
//! isolation.
//!
//! ## Conventions (documented latitude)
//!
//! - **Section order** is the fixed canonical order [`CANONICAL_ORDER`]
//!   (`Added, Changed, Deprecated, Removed, Fixed, Security, Internal`); only
//!   non-empty sections are emitted.
//! - **Migration notes** render as an italic `_Migration:_ …` line indented
//!   under their bullet (not a separate `### Breaking changes` block), so the
//!   note travels with the change it explains.
//! - **Issue refs** are appended to the bullet in parentheses, e.g. `(#987)` or
//!   `(#987, #990)`.
//! - A crate that bumped **only transitively** (no direct entries) renders its
//!   dependency-bump reasons as bullets under `### Changed`.
//! - **Insertion anchor**: the new section lands immediately before the first
//!   *released-version* heading (a `## ` heading whose text — after an optional
//!   `[` — starts with a digit), so the file title, preamble, and any
//!   `## [Unreleased]` block are preserved above it. With no such heading the
//!   section is appended; a missing file is created with a Keep-a-Changelog
//!   preamble.

use relman_core::types::{
    BumpTable, ChangeEntry, CrateBump, CycleId, CycleStatus, NaiveDate, Section, Tag,
};

/// The order sections are emitted in, regardless of the order changes were
/// authored. Keep-a-Changelog's canonical order, with `Internal` last.
pub(crate) const CANONICAL_ORDER: [Section; 7] = [
    Section::Added,
    Section::Changed,
    Section::Deprecated,
    Section::Removed,
    Section::Fixed,
    Section::Security,
    Section::Internal,
];

/// The preamble a freshly-created changelog opens with, matching the house
/// style of the existing per-crate changelogs.
const NEW_FILE_PREAMBLE: &str = "\
# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
";

/// Format a date as `YYYY-MM-DD` for a version heading.
fn iso(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// The `- bullet` line(s) for one direct entry: the description with any issue
/// refs, plus an indented `_Migration:_` line when the entry carries one.
fn entry_lines(entry: &ChangeEntry) -> Vec<String> {
    let mut bullet = format!("- {}", entry.description().as_str());
    if !entry.issues().is_empty() {
        bullet.push_str(&format!(" ({})", entry.issues().join(", ")));
    }
    let mut lines = vec![bullet];
    if let Some(migration) = entry.migration() {
        lines.push(format!("  _Migration:_ {migration}"));
    }
    lines
}

/// Group a crate's change bullets by section in [`CANONICAL_ORDER`], dropping
/// empty sections. Direct entries land under their `effective_section`;
/// transitive dependency-bump reasons (those in `bump.reasons()` after the
/// direct ones) land under `Changed`.
fn grouped_bullets(bump: &CrateBump, direct: &[&ChangeEntry]) -> Vec<(Section, Vec<String>)> {
    let mut buckets: Vec<(Section, Vec<String>)> =
        CANONICAL_ORDER.iter().map(|s| (*s, Vec::new())).collect();

    let push = |buckets: &mut Vec<(Section, Vec<String>)>, section: Section, lines: Vec<String>| {
        if let Some((_, bucket)) = buckets.iter_mut().find(|(s, _)| *s == section) {
            bucket.extend(lines);
        }
    };

    for entry in direct {
        push(&mut buckets, entry.effective_section(), entry_lines(entry));
    }

    // Reasons beyond the direct descriptions are the transitive dependency-bump
    // explanations; they read as `Changed`.
    let reasons = bump.reasons();
    if reasons.len() > direct.len() {
        let transitive = reasons[direct.len()..]
            .iter()
            .map(|r| format!("- {r}"))
            .collect();
        push(&mut buckets, Section::Changed, transitive);
    }

    buckets.into_iter().filter(|(_, l)| !l.is_empty()).collect()
}

/// Render one crate's changelog section: the `## [X.Y.Z] - DATE` heading
/// followed by its `### Section` groups. Ends with a trailing newline.
pub(crate) fn render_crate_section(
    bump: &CrateBump,
    direct: &[&ChangeEntry],
    date: NaiveDate,
) -> String {
    let mut lines = vec![format!("## [{}] - {}", bump.next(), iso(date))];
    for (section, bullets) in grouped_bullets(bump, direct) {
        lines.push(format!("### {}", section.heading()));
        lines.extend(bullets);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Render the date-free aggregated changelog digest: one `### <crate> <version>`
/// subsection per bumping crate carrying that crate's flattened bullets. Ends
/// with a trailing newline.
///
/// Each element pairs a bumping crate with its direct entries (empty for a
/// transitive-only crate), in the order the subsections should appear. This is
/// the body shared by the workspace changelog section (which prepends a dated
/// heading) and the release-PR changelog block (which needs no date).
pub(crate) fn render_changelog_digest(crates: &[(&CrateBump, Vec<&ChangeEntry>)]) -> String {
    let mut lines = Vec::new();
    for (bump, direct) in crates {
        lines.push(format!(
            "### {} {}",
            bump.crate_name().as_str(),
            bump.next()
        ));
        for (_, bullets) in grouped_bullets(bump, direct) {
            lines.extend(bullets);
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Render the workspace changelog section: a date-labelled `## [DATE]` heading
/// above the [`render_changelog_digest`] body. Ends with a trailing newline.
pub(crate) fn render_workspace_section(
    crates: &[(&CrateBump, Vec<&ChangeEntry>)],
    date: NaiveDate,
) -> String {
    format!("## [{}]\n{}", iso(date), render_changelog_digest(crates))
}

/// The placeholder for an empty marker/cell, so every table cell is non-blank.
const EMPTY_CELL: &str = "—";

/// The trailing `rc.<M>` label of a prerelease tag (e.g. `cycle-1-rc.1` →
/// `rc.1`). Falls back to the whole tag if it carries no `-rc.` segment.
fn rc_label(tag: &str) -> &str {
    match tag.rfind("-rc.") {
        Some(idx) => &tag[idx + 1..],
        None => tag,
    }
}

/// The numeric `<M>` of a prerelease tag's `rc.<M>` suffix, for ordering RCs by
/// recency independent of the input order. A tag with no parseable suffix
/// sorts as `0`.
fn rc_number(tag: &str) -> u32 {
    tag.rfind("-rc.")
        .and_then(|idx| tag[idx + 4..].parse().ok())
        .unwrap_or(0)
}

/// The most recent release-candidate tag in `status` — the `[[rc]]` entry with
/// the highest `rc.<M>` number, chosen deterministically regardless of the
/// order the entries were listed in.
fn latest_rc_tag(status: &CycleStatus) -> Option<&Tag> {
    status
        .rc()
        .iter()
        .max_by_key(|entry| rc_number(entry.tag().as_str()))
        .map(|entry| entry.tag())
}

/// Render the `## Gate watermarks` table: one row per gate whose branch tip is
/// known, mapping each gate to its branch, its commit, and a marker. Ends with a
/// trailing newline. Rows for absent watermarks are omitted.
///
/// Marker column: `stable` shows the released cycle tag; `rc` shows the latest
/// prerelease tag; the remaining gates carry a placeholder.
pub(crate) fn render_gate_watermarks(status: &CycleStatus) -> String {
    let wm = status.watermarks();
    let released = status
        .released_cycle()
        .map(|t| t.as_str().to_owned())
        .unwrap_or_else(|| EMPTY_CELL.to_owned());
    let latest_rc = latest_rc_tag(status)
        .map(|t| t.as_str().to_owned())
        .unwrap_or_else(|| EMPTY_CELL.to_owned());

    // (gate, branch, commit, marker) — only rows whose commit is present render.
    let rows = [
        ("verified", "dev", wm.dev(), EMPTY_CELL.to_owned()),
        ("candidate", "rc", wm.rc(), latest_rc),
        (
            "release-ready",
            "release-ready",
            wm.release_ready(),
            EMPTY_CELL.to_owned(),
        ),
        ("released", "stable", wm.stable(), released),
    ];

    let mut out = String::from("## Gate watermarks\n\n");
    out.push_str("| Gate | Branch | Commit | Marker |\n");
    out.push_str("| ---- | ------ | ------ | ------ |\n");
    for (gate, branch, commit, marker) in rows {
        let Some(commit) = commit else {
            continue;
        };
        out.push_str(&format!("| {gate} | {branch} | {commit} | {marker} |\n"));
    }
    out
}

/// Render the `## Release candidates — cycle <N>` table from the `[[rc]]`
/// entries, in listed order: `| RC | Commit | Tag | Deployment |`, deployment as
/// a glyph + word. Ends with a trailing newline; an empty candidate list renders
/// a note instead of an empty table.
pub(crate) fn render_rc_table(cycle: &CycleId, status: &CycleStatus) -> String {
    let mut out = format!("## Release candidates — cycle {cycle}\n\n");
    if status.rc().is_empty() {
        out.push_str("_No release candidates cut yet this cycle._\n");
        return out;
    }
    out.push_str("| RC | Commit | Tag | Deployment |\n");
    out.push_str("| -- | ------ | --- | ---------- |\n");
    for entry in status.rc() {
        let deployment = entry.deployment();
        out.push_str(&format!(
            "| {} | {} | {} | {} {} |\n",
            rc_label(entry.tag().as_str()),
            entry.sha(),
            entry.tag(),
            deployment.glyph(),
            deployment.as_str(),
        ));
    }
    out
}

/// Render the `## Version bumps (derived, since last stable)` table. Ends with a
/// trailing newline.
///
/// With `with_tags`, a per-target `Tag` column carries each bumping crate's
/// `<crate>-v<next>` provenance tag (the tag CI applies at blessing); without
/// it, the classic four-column table.
pub(crate) fn render_version_table(table: &BumpTable, with_tags: bool) -> String {
    let mut out = String::from("## Version bumps (derived, since last stable)\n\n");
    if with_tags {
        out.push_str("| Crate | Current | Next | Bump | Tag |\n");
        out.push_str("| ----- | ------- | ---- | ---- | --- |\n");
    } else {
        out.push_str("| Crate | Current | Next | Bump |\n");
        out.push_str("| ----- | ------- | ---- | ---- |\n");
    }
    for bump in table.bumps() {
        if with_tags {
            let tag = Tag::crate_version(bump.crate_name(), bump.next());
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                bump.crate_name(),
                bump.current(),
                bump.next(),
                bump.bump().as_str(),
                tag,
            ));
        } else {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                bump.crate_name(),
                bump.current(),
                bump.next(),
                bump.bump().as_str(),
            ));
        }
    }
    out
}

/// Whether `line` is a *released-version* heading — a `## ` heading whose text,
/// after an optional `[`, begins with a digit (e.g. `## [0.6.0] - …`). An
/// `## [Unreleased]` / `## Unreleased` heading is deliberately not one, so it
/// stays above the inserted section.
fn is_version_heading(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("## ") else {
        return false;
    };
    let rest = rest.strip_prefix('[').unwrap_or(rest);
    rest.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Splice `section` (a trailing-newline-terminated block from
/// [`render_crate_section`] / [`render_workspace_section`]) into `existing`
/// just before the first released-version heading, preserving everything above.
///
/// When `existing` is `None` the file is created with a Keep-a-Changelog
/// preamble; when it has no released-version heading the section is appended.
pub(crate) fn insert_section(existing: Option<&str>, section: &str) -> String {
    let Some(existing) = existing else {
        return format!("{NEW_FILE_PREAMBLE}\n{section}");
    };

    match existing.lines().position(is_version_heading) {
        Some(idx) => {
            // Byte offset of the anchor line's start.
            let mut offset = 0usize;
            for line in existing.lines().take(idx) {
                offset += line.len() + 1; // +1 for the '\n' the split dropped
            }
            let (before, after) = existing.split_at(offset);
            format!("{before}{section}\n{after}")
        }
        None => {
            // No version history yet — append after the preamble/unreleased.
            format!("{}\n\n{section}", existing.trim_end())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use relman_core::types::{Bump, ChangeKind, CrateName, Description, Version};

    fn name(raw: &str) -> CrateName {
        CrateName::parse(raw).expect("valid crate name")
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).expect("valid version")
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 19).expect("valid date")
    }

    fn entry(
        kind: ChangeKind,
        desc: &str,
        section: Option<Section>,
        migration: Option<&str>,
        issues: &[&str],
    ) -> ChangeEntry {
        ChangeEntry::new(
            name("zaino-state"),
            kind,
            Description::parse(desc).expect("non-empty"),
            section,
            migration.map(str::to_owned),
            issues.iter().map(|s| (*s).to_owned()).collect(),
        )
    }

    #[test]
    fn breaking_with_migration_and_fix_renders_exact_section() {
        let breaking = entry(
            ChangeKind::Breaking,
            "Replace `sync()` with `sync_with(SyncMode)`.",
            None,
            Some("Call `sync_with(SyncMode::Serial)` for the old behaviour."),
            &[],
        );
        let fix = entry(
            ChangeKind::Fix,
            "Stop double-counting orphaned blocks.",
            None,
            None,
            &[],
        );
        let bump = CrateBump::new(
            name("zaino-state"),
            version("0.6.0"),
            version("0.7.0"),
            Bump::Minor,
            vec![
                "Replace `sync()` with `sync_with(SyncMode)`.".to_owned(),
                "Stop double-counting orphaned blocks.".to_owned(),
            ],
        );

        let section = render_crate_section(&bump, &[&breaking, &fix], date());
        let expected = "\
## [0.7.0] - 2026-08-19
### Changed
- Replace `sync()` with `sync_with(SyncMode)`.
  _Migration:_ Call `sync_with(SyncMode::Serial)` for the old behaviour.
### Fixed
- Stop double-counting orphaned blocks.
";
        assert_eq!(section, expected);
    }

    #[test]
    fn section_override_and_issues_and_internal() {
        // A feature forced into Removed, an internal, and a fix with issue refs
        // — exercising the override, the Internal heading, and issue appending,
        // and confirming canonical ordering (Removed before Fixed).
        let removed = entry(
            ChangeKind::Feature,
            "Drop the legacy path.",
            Some(Section::Removed),
            None,
            &[],
        );
        let internal = entry(ChangeKind::Internal, "Refactor plumbing.", None, None, &[]);
        let fix = entry(
            ChangeKind::Fix,
            "Fix a gauge.",
            None,
            None,
            &["#987", "#990"],
        );
        let bump = CrateBump::new(
            name("zaino-state"),
            version("0.6.0"),
            version("0.6.1"),
            Bump::Patch,
            vec![
                "Drop the legacy path.".to_owned(),
                "Refactor plumbing.".to_owned(),
                "Fix a gauge.".to_owned(),
            ],
        );

        let section = render_crate_section(&bump, &[&removed, &internal, &fix], date());
        let expected = "\
## [0.6.1] - 2026-08-19
### Removed
- Drop the legacy path.
### Fixed
- Fix a gauge. (#987, #990)
### Internal
- Refactor plumbing.
";
        assert_eq!(section, expected);
    }

    #[test]
    fn transitive_only_crate_emits_single_changed_bullet() {
        // No direct entries; the one reason is a transitive dependency bump.
        let bump = CrateBump::new(
            name("zainod"),
            version("0.4.3"),
            version("0.4.4"),
            Bump::Patch,
            vec!["dependency `zaino-state` 0.6.0→0.7.0 crossed the requirement `^0.6`".to_owned()],
        );
        let section = render_crate_section(&bump, &[], date());
        let expected = "\
## [0.4.4] - 2026-08-19
### Changed
- dependency `zaino-state` 0.6.0→0.7.0 crossed the requirement `^0.6`
";
        assert_eq!(section, expected);
    }

    #[test]
    fn workspace_section_lists_each_crate_with_bullets() {
        let state_bump = CrateBump::new(
            name("zaino-state"),
            version("0.6.0"),
            version("0.6.1"),
            Bump::Patch,
            vec!["Fix a gauge.".to_owned()],
        );
        let state_fix = entry(ChangeKind::Fix, "Fix a gauge.", None, None, &[]);

        let daemon_bump = CrateBump::new(
            name("zainod"),
            version("0.4.3"),
            version("0.4.4"),
            Bump::Patch,
            vec![
                "dependency `zaino-state` 0.6.0→0.6.1 crossed the requirement `=0.6.0`".to_owned(),
            ],
        );

        let crates = vec![(&state_bump, vec![&state_fix]), (&daemon_bump, Vec::new())];
        let section = render_workspace_section(&crates, date());
        let expected = "\
## [2026-08-19]
### zaino-state 0.6.1
- Fix a gauge.
### zainod 0.4.4
- dependency `zaino-state` 0.6.0→0.6.1 crossed the requirement `=0.6.0`
";
        assert_eq!(section, expected);
    }

    #[test]
    fn inserts_between_unreleased_and_prior_version() {
        let existing = "\
# Changelog
All notable changes to this library will be documented in this file.

## [Unreleased]

### Added

## [0.6.0] - 2026-08-04

### Fixed
- An old fix.
";
        let section = "\
## [0.7.0] - 2026-08-19
### Fixed
- A new fix.
";
        let result = insert_section(Some(existing), section);
        let expected = "\
# Changelog
All notable changes to this library will be documented in this file.

## [Unreleased]

### Added

## [0.7.0] - 2026-08-19
### Fixed
- A new fix.

## [0.6.0] - 2026-08-04

### Fixed
- An old fix.
";
        assert_eq!(result, expected);
    }

    #[test]
    fn missing_file_is_created_with_preamble() {
        let section = "\
## [0.1.0] - 2026-08-19
### Added
- First release.
";
        let result = insert_section(None, section);
        assert!(result.starts_with("# Changelog\n"));
        assert!(result.contains("## [Unreleased]"));
        assert!(result.trim_end().ends_with("- First release."));
        // The version section sits below the Unreleased block.
        let unreleased = result.find("## [Unreleased]").expect("has unreleased");
        let version = result.find("## [0.1.0]").expect("has version");
        assert!(unreleased < version);
    }

    #[test]
    fn appends_when_no_version_history_exists() {
        let existing = "\
# Changelog

## [Unreleased]
### Added
";
        let section = "\
## [0.1.0] - 2026-08-19
### Added
- First.
";
        let result = insert_section(Some(existing), section);
        assert!(result.contains("## [Unreleased]"));
        assert!(result.trim_end().ends_with("- First."));
    }

    fn cycle(raw: &str) -> CycleId {
        CycleId::parse(raw).expect("valid cycle id")
    }

    fn status(toml: &str) -> CycleStatus {
        CycleStatus::parse_toml(toml).expect("valid status")
    }

    const FULL_STATUS: &str = "\
released_cycle = \"cycle-0\"

[watermarks]
dev = \"dd73705\"
rc = \"dd73705\"
release_ready = \"aa11bb2\"
stable = \"5e3caa1\"

[[rc]]
tag = \"cycle-1-rc.2\"
sha = \"aa11bb2\"
deployment = \"running\"

[[rc]]
tag = \"cycle-1-rc.1\"
sha = \"dd73705\"
deployment = \"passed\"
";

    #[test]
    fn gate_watermarks_renders_exact_table() {
        let expected = "\
## Gate watermarks

| Gate | Branch | Commit | Marker |
| ---- | ------ | ------ | ------ |
| verified | dev | dd73705 | — |
| candidate | rc | dd73705 | cycle-1-rc.2 |
| release-ready | release-ready | aa11bb2 | — |
| released | stable | 5e3caa1 | cycle-0 |
";
        // The `rc` marker is the *latest* rc tag (rc.2), chosen by number even
        // though rc.1 was listed after it.
        assert_eq!(render_gate_watermarks(&status(FULL_STATUS)), expected);
    }

    #[test]
    fn gate_watermarks_omits_absent_branches() {
        // Only dev and stable exist yet; rc / release-ready rows are omitted,
        // and with no rc entries the (absent) rc row's marker never matters.
        let toml = "\
[watermarks]
dev = \"abc1234\"
stable = \"def5678\"
";
        let expected = "\
## Gate watermarks

| Gate | Branch | Commit | Marker |
| ---- | ------ | ------ | ------ |
| verified | dev | abc1234 | — |
| released | stable | def5678 | — |
";
        assert_eq!(render_gate_watermarks(&status(toml)), expected);
    }

    #[test]
    fn rc_table_renders_exact_rows_with_glyphs() {
        let expected = "\
## Release candidates — cycle 1

| RC | Commit | Tag | Deployment |
| -- | ------ | --- | ---------- |
| rc.2 | aa11bb2 | cycle-1-rc.2 | ● running |
| rc.1 | dd73705 | cycle-1-rc.1 | ✓ passed |
";
        // Listed order is preserved (rc.2 then rc.1).
        assert_eq!(render_rc_table(&cycle("1"), &status(FULL_STATUS)), expected);
    }

    #[test]
    fn rc_table_notes_when_no_candidates() {
        let toml = "[watermarks]\ndev = \"abc1234\"\n";
        let expected = "\
## Release candidates — cycle 1

_No release candidates cut yet this cycle._
";
        assert_eq!(render_rc_table(&cycle("1"), &status(toml)), expected);
    }

    #[test]
    fn version_table_with_tags_renders_per_target_tag_column() {
        let table = BumpTable::new(vec![
            CrateBump::new(
                name("zaino-state"),
                version("0.6.0"),
                version("0.7.0"),
                Bump::Minor,
                vec!["x".to_owned()],
            ),
            CrateBump::new(
                name("zainod"),
                version("0.4.3"),
                version("0.4.4"),
                Bump::Patch,
                vec!["y".to_owned()],
            ),
        ]);
        let expected = "\
## Version bumps (derived, since last stable)

| Crate | Current | Next | Bump | Tag |
| ----- | ------- | ---- | ---- | --- |
| zaino-state | 0.6.0 | 0.7.0 | minor | zaino-state-v0.7.0 |
| zainod | 0.4.3 | 0.4.4 | patch | zainod-v0.4.4 |
";
        assert_eq!(render_version_table(&table, true), expected);
    }

    #[test]
    fn version_table_without_tags_keeps_classic_four_columns() {
        let table = BumpTable::new(vec![CrateBump::new(
            name("zaino-state"),
            version("0.6.0"),
            version("0.7.0"),
            Bump::Minor,
            vec!["x".to_owned()],
        )]);
        let expected = "\
## Version bumps (derived, since last stable)

| Crate | Current | Next | Bump |
| ----- | ------- | ---- | ---- |
| zaino-state | 0.6.0 | 0.7.0 | minor |
";
        assert_eq!(render_version_table(&table, false), expected);
    }

    #[test]
    fn is_version_heading_distinguishes_unreleased() {
        assert!(is_version_heading("## [0.6.0] - 2026-08-04"));
        assert!(is_version_heading("## 0.6.0"));
        assert!(!is_version_heading("## [Unreleased]"));
        assert!(!is_version_heading("## Unreleased"));
        assert!(!is_version_heading("### Added"));
        assert!(!is_version_heading("# Changelog"));
    }
}
