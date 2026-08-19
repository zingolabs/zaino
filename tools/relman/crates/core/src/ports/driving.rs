use crate::types::AboutReport;

/// Inbound port: report who relman is (version) and what it thinks "now" is.
///
/// Implemented by the domain (`AboutService`) and consumed by delivery
/// mechanisms through `Arc<dyn About>`. Callers never name the concrete
/// service — only the binary's composition root does. This is the trivial
/// live thread that keeps every seam exercised; the real driving ports
/// (`Changesets`, `Versions`, `Bump`, `Changelog`, `ReleaseArtifacts`) arrive
/// in later slices.
pub trait About: Send + Sync {
    fn report(&self) -> AboutReport;
}
