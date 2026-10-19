//! `method` + `code` label tables shared by `metrics` and `summary` (slots derive from `ROUTES`)

use crate::service::{Method, ROUTES};

/// `method` label slots: one per [`ROUTES`] row, then `unknown` (any other path: the set stays
/// fixed however clients misbehave)
pub(crate) const LABELS: usize = ROUTES.len() + 1;

/// `method`'s label slot (`None` = `unknown`)
pub(crate) fn slot(method: Option<Method>) -> usize {
    method.map_or(ROUTES.len(), |method| method as usize)
}

pub(crate) fn label(slot: usize) -> &'static str {
    ROUTES.get(slot).map_or("unknown", |row| row.name)
}

/// `Code`'s `Debug` spelling by value (the `code` label, never formatted per request)
pub(crate) const CODES: [&str; 17] = [
    "Ok",
    "Cancelled",
    "Unknown",
    "InvalidArgument",
    "DeadlineExceeded",
    "NotFound",
    "AlreadyExists",
    "PermissionDenied",
    "ResourceExhausted",
    "FailedPrecondition",
    "Aborted",
    "OutOfRange",
    "Unimplemented",
    "Internal",
    "Unavailable",
    "DataLoss",
    "Unauthenticated",
];
