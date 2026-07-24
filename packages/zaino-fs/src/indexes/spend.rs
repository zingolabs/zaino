//! Addon index: spend status of a transparent outpoint.

use std::future::Future;

use zaino_core::{Outpoint, SpendStatus};

use crate::error::LookupError;

/// Whether/where a transparent outpoint was spent. Backed by the spent-outpoint
/// index. `NoSuchOutput`/unspent are domain answers, not errors; only the
/// backend can fail.
pub trait SpendIndex: Send + Sync {
    fn spend_status(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<SpendStatus, LookupError>> + Send;
}
