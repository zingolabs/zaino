//! Presentation helpers: turn core types into terminal output. Keeping this
//! separate from command logic makes both easy to change independently.

use relman_core::types::{AboutReport, BumpTable, PublishPlan, TagPlan};

pub fn about(report: &AboutReport) -> String {
    format!(
        "relman:   {}\nnow:      {}",
        report.version,
        report.now.to_rfc3339(),
    )
}

/// Render the derived bump table: one row per bumping crate as
/// `crate  current → next  (bump)`, each followed by its indented reasons, or a
/// note when nothing bumps.
pub fn bump_table(table: &BumpTable) -> String {
    if table.is_empty() {
        return "relman: nothing bumps (no changesets affect a governed target)\n".to_owned();
    }

    // Left-align the crate column to the widest name for a readable table.
    let name_width = table
        .bumps()
        .iter()
        .map(|b| b.crate_name().as_str().len())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for bump in table.bumps() {
        out.push_str(&format!(
            "{:<name_width$}  {} → {}  ({})\n",
            bump.crate_name().as_str(),
            bump.current(),
            bump.next(),
            bump.bump().as_str(),
        ));
        for reason in bump.reasons() {
            out.push_str(&format!("    - {reason}\n"));
        }
    }
    out
}

/// Render a tag plan: one tag name per line, for CI to `git tag` verbatim.
pub fn tag_plan(plan: &TagPlan) -> String {
    let mut out = String::new();
    for tag in plan.tags() {
        out.push_str(tag.as_str());
        out.push('\n');
    }
    out
}

/// Render a publish plan: one `crate version` per line, in publish order, and
/// nothing at all for an empty plan, since CI parses every line as a crate.
pub fn publish_plan(plan: &PublishPlan) -> String {
    let mut out = String::new();
    for (name, version) in plan.entries() {
        out.push_str(&format!("{name} {version}\n"));
    }
    out
}

#[cfg(test)]
mod publish_plan {
    use super::*;

    /// Every stdout line of `publish-plan` is parsed by CI as `<crate> <version>`,
    /// so an empty plan must render as no lines at all — exactly as `tag_plan`
    /// does — never as a prose sentence a shell loop would read as a crate name.
    #[test]
    fn renders_nothing_for_an_empty_plan() {
        assert_eq!(publish_plan(&PublishPlan::default()), "");
        assert_eq!(tag_plan(&TagPlan::default()), "");
    }
}
