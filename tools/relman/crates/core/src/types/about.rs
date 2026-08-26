use crate::types::{DateTime, Utc};

/// The result of the `about` query — the output of the [`About`] driving port.
///
/// A deliberately tiny value type: it carries relman's own build version and
/// the current instant (from the [`Clock`] driven port). It exists to prove
/// the hexagon compiles end-to-end; later slices add the real release types.
///
/// [`About`]: crate::ports::About
/// [`Clock`]: crate::ports::Clock
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AboutReport {
    pub version: &'static str,
    pub now: DateTime<Utc>,
}
