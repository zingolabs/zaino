//! The transparent UTXO-set summary over the finalised range.
//!
//! The `txout_set` index holds what each block added to and removed from the
//! summary, so the summary at the watermark is the fold of every entry up to
//! it. This read does that fold on each call: one ordered scan of the
//! namespace, off the async executor because it is scan-shaped.

use std::ops::ControlFlow;

use zaino_indexes::index_set::Builds;
use zaino_indexes::indexes::txout_set::{self, TxOutSetDelta, TxOutSetIndex};
use zaino_persistence::{Backend, BackendReader, Namespace};
use zaino_persistence_codec::{decode_value, encode_key};
use zaino_primitives::types::{BlockRef, TxOutSetInfo, Zatoshis};
use zaino_service::error::ReadError;
use zaino_service::TxOutSetRead;
use zaino_sync::primitives::BlockHeight;

use crate::StoreSnapshot;

/// The store answers the UTXO-set summary wherever its index set builds the
/// `txout_set` index.
impl<B, M> TxOutSetRead for StoreSnapshot<B, M>
where
    B: Backend + Send + Sync + 'static,
    M: Builds<TxOutSetIndex>,
{
    async fn tx_out_set_info(&self) -> Result<Option<TxOutSetInfo>, ReadError> {
        // Nothing finalised yet: there is no height to describe.
        let Some(tip) = self.pinned_tip else {
            return Ok(None);
        };
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || summary_at::<B>(&backend, tip))
            .await
            .map_err(|e| ReadError::Fatal(format!("UTXO-set summary task failed: {e}")))?
            .map(Some)
    }
}

/// Fold the deltas of `[genesis, tip]` into the summary at `tip`.
fn summary_at<B: Backend>(backend: &B, tip: BlockRef) -> Result<TxOutSetInfo, ReadError> {
    let reader = backend
        .reader()
        .map_err(|e| ReadError::Fatal(format!("open reader: {e}")))?;
    let namespace: Namespace = txout_set::ID.into();
    let tip_height = BlockHeight::new(u64::from(tip.height));
    let start = encode_key::<TxOutSetIndex>(&BlockHeight::new(0));
    let end = encode_key::<TxOutSetIndex>(&BlockHeight::new(tip_height.value() + 1));

    let mut sum = TxOutSetDelta::EMPTY;
    let mut failure: Option<ReadError> = None;
    let mut entries: u64 = 0;
    reader
        .scan_range(namespace, &start, &end, &mut |_key, value| {
            let step = decode_value::<TxOutSetIndex>(value)
                .map_err(|e| ReadError::Fatal(format!("decode UTXO-set delta: {e}")))
                .and_then(|delta| {
                    sum.combine(delta)
                        .map_err(|e| ReadError::Fatal(e.to_string()))
                });
            match step {
                Ok(next) => {
                    sum = next;
                    entries += 1;
                    ControlFlow::Continue(())
                }
                Err(error) => {
                    failure = Some(error);
                    ControlFlow::Break(())
                }
            }
        })
        .map_err(|e| ReadError::Transient(format!("scan UTXO-set deltas: {e}")))?;
    if let Some(error) = failure {
        return Err(error);
    }

    // The index is walk-ordered, so it holds one entry per height up to the
    // watermark. A shorter scan means the summary would silently omit blocks.
    let expected = tip_height.value() + 1;
    if entries != expected {
        return Err(ReadError::Fatal(format!(
            "UTXO-set summary holds {entries} of {expected} heights at or below the watermark"
        )));
    }

    let totals = sum.totals().map_err(|e| ReadError::Fatal(e.to_string()))?;
    Ok(TxOutSetInfo {
        height: tip.height,
        best_block: tip.hash,
        transactions: totals.transactions,
        tx_outs: totals.tx_outs,
        bytes_serialized: totals.bytes_serialized,
        hash_serialized: hex(&totals.hash_serialized),
        total_amount: Zatoshis::new(totals.total_zatoshis)
            .map_err(|e| ReadError::Fatal(format!("UTXO-set total out of range: {e}")))?,
    })
}

/// Lower-case hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
