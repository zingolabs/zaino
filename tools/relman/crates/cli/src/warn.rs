//! Stderr warnings the CLI surfaces around the aggregation commands.
//!
//! Keeping this here (not in the domain services) preserves the rule that
//! services never touch stdout/stderr: they *report* skippable state as data,
//! and this delivery adapter turns it into a user-facing warning.

use crate::context::Ctx;

/// Warn on stderr about every unfilled changeset template in the store.
///
/// An unfilled template is a `relman changeset new` scaffold the author never
/// edited; every aggregation command skips it, so we name the skipped file.
/// Best-effort: if the scan itself fails (store I/O), we stay silent — the
/// command's own read of the store surfaces the real error.
pub(crate) fn unfilled_templates(ctx: &Ctx) {
    let Ok(paths) = ctx.versions.unfilled_templates() else {
        return;
    };
    for path in paths {
        eprintln!(
            "relman: warning: skipping unfilled changeset template {}",
            path.display()
        );
    }
}
