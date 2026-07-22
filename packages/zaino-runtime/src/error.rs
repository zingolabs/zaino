//! Runtime lifecycle errors.

/// Runtime lifecycle errors (placeholder).
#[derive(Debug)]
pub enum RuntimeError {
    /// Boot / bulk-build failure.
    Init(String),
}
