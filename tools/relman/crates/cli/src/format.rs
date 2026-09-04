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

/// Render a publish plan: one `crate version` per line, in publish order.
pub fn publish_plan(plan: &PublishPlan) -> String {
    if plan.is_empty() {
        return "relman: nothing to publish (no changesets affect a governed target)\n".to_owned();
    }
    let mut out = String::new();
    for (name, version) in plan.entries() {
        out.push_str(&format!("{name} {version}\n"));
    }
    out
}
