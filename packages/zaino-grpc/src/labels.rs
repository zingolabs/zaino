//! `method` + `code` label tables shared by `metrics` and `summary` (slots = [`Method`] order)

use strum::VariantArray as _;

use crate::service::Method;

/// `method` label slots: one per [`Method`], then `unknown` (any other path: the set stays fixed
/// however clients misbehave)
pub(crate) const LABELS: usize = Method::VARIANTS.len() + 1;

/// `method`'s label slot (`None` = `unknown`)
pub(crate) fn slot(method: Option<Method>) -> usize {
    method.map_or(Method::VARIANTS.len(), |method| method as usize)
}

pub(crate) fn label(slot: usize) -> &'static str {
    Method::VARIANTS.get(slot).map_or("unknown", |method| method.name())
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
