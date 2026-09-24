//! Query: fetch one mempool transaction, projected to the compact serving form.

use std::future::Future;

use zaino_primitives::types::{PreIndexCompactTx, TransactionId};

use super::get_raw_mempool_transaction::GetRawMempoolTransactionError;
use super::{QueryError, ValidatorSource};

/// Fetch one mempool transaction already projected to [`PreIndexCompactTx`].
///
/// The compact projection deserialises the raw transaction and keeps only the
/// fields a light wallet scans. It is a source-adapter concern — the projection
/// needs the validator's chain library, which the serving layer must not depend
/// on — so it lives here beside
/// [`GetRawMempoolTransaction`](super::GetRawMempoolTransaction) and shares its
/// miss: a txid dropped between listing and fetch is
/// [`NotFound`](GetRawMempoolTransactionError::NotFound), a race, not a failure.
///
/// Like the other mempool reads, an implementation must route this to the same
/// source that serves [`GetMempoolTxids`](super::GetMempoolTxids), so the compact
/// bytes and the listing they belong to come from one source.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetMempoolCompactTransaction: ValidatorSource + Send + Sync {
    /// Fetch one mempool transaction's compact projection.
    fn get_mempool_compact_transaction(
        &self,
        txid: TransactionId,
    ) -> impl Future<
        Output = Result<
            PreIndexCompactTx,
            QueryError<GetRawMempoolTransactionError, Self::NonDomain>,
        >,
    > + Send;
}
