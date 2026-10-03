//! Query: ping the validator.

use std::convert::Infallible;
use std::future::Future;

use super::{QueryError, ValidatorSource};

/// Ask the validator to confirm it is responsive.
///
/// There is no domain answer — the validator either responds or it does not —
/// so the only failure is a non-domain transport one (hence [`Infallible`] as the
/// domain error). zcashd/zebra answer a successful `ping` with a JSON `null`.
///
/// Maps to `ping` over JSON-RPC.
#[zaino_source_macros::resilient_port]
pub trait OneShotPing: ValidatorSource + Send + Sync {
    /// Ping the validator; `Ok(())` on a successful response.
    fn ping(
        &self,
    ) -> impl Future<Output = Result<(), QueryError<Infallible, Self::NonDomain>>> + Send;
}
