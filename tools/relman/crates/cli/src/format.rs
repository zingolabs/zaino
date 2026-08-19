//! Presentation helpers: turn core types into terminal output. Keeping this
//! separate from command logic makes both easy to change independently.

use relman_core::types::AboutReport;

pub fn about(report: &AboutReport) -> String {
    format!(
        "relman:   {}\nnow:      {}",
        report.version,
        report.now.to_rfc3339(),
    )
}
