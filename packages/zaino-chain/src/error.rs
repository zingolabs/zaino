//! What can go wrong asking a chain view a question.

use zaino_source::FetchError;

/// A chain view could not answer.
///
/// # A miss is not an error
///
/// The rule the whole crate is written to: a question whose answer is "nothing"
/// returns `Ok(None)` or an empty collection. A block that does not exist, a
/// transaction never mined, an address never paid — none are failures, and
/// phrasing them as failures is how a consumer comes to treat absent *data* as
/// absent *chain*.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChainViewError {
    /// The view does not offer this, or cannot reach that far.
    ///
    /// Not a miss and not a failure: the view is healthy and the question is
    /// well-formed. Retrying is pointless — an index is not built, the
    /// validator is switched off, or the providers leave a hole that nothing
    /// can fill. The message names which.
    #[error("chain view cannot service this read: {0}")]
    NotServiceable(&'static str),

    /// The validator could not be reached. A retry may succeed.
    ///
    /// Carries the transport [`FetchError`] as its `#[source]`, so
    /// [`Error::source`](std::error::Error::source) yields the underlying cause
    /// — and with it the machine-readable [`FailureMode`](zaino_source::FailureMode)
    /// — rather than a flattened string.
    #[error("validator unavailable: {0}")]
    SourceUnavailable(#[source] FetchError),

    /// A retry may succeed: a tier was briefly between states.
    ///
    /// Carries a description rather than the underlying error, because a chain
    /// view sits above several unrelated backends and preserving each one's
    /// error type here would put every backend's vocabulary in this crate's
    /// public API.
    #[error("chain view read failed transiently: {0}")]
    Transient(String),

    /// A backend failed in a way that will not resolve on its own.
    #[error("chain view read failed: {0}")]
    Fatal(String),
}

/// The result of a chain view read.
pub type Result<T> = core::result::Result<T, ChainViewError>;
