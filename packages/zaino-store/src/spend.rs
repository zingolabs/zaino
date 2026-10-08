//! Spend status over the finalised store.
//!
//! The finalised half of the `SpendStatus` capability: whether, and by which
//! transaction, a transparent outpoint was spent at or below the watermark.
//!
//! # Three states, three indexes
//!
//! The spends index answers only "was this spent, and by whom". Absence from
//! it is ambiguous — an outpoint the finalised range never created is absent
//! exactly as an unspent one is — so absence is resolved against the output's
//! existence: the txid's location, then that block's transparent data. That is
//! why [`local::SpendStatus`] names three indexes and this read bounds on all
//! of them.
//!
//! ```text
//! spent(o)            → Spent { by }
//! ¬spent(o) ∧ ∃o      → Unspent
//! ¬spent(o) ∧ ¬∃o     → NoSuchOutput
//! ```
//!
//! `Unspent` here means "unspent as of the watermark". The volatile window
//! above it may have spent the output since, which is the composer's business,
//! not this tier's: `zaino_core`'s `Local` spend placement asks the head first
//! for exactly that reason.

use zaino_indexes::capabilities::local::{self, Backs};
use zaino_indexes::indexes::transparent_data::{self, TransparentDataIndex};
use zaino_indexes::indexes::transparent_spends::{self, read_spender, OutpointKey};
use zaino_indexes::indexes::txid_location::{self, TxidLocationIndex};
use zaino_persistence::Backend;
use zaino_primitives::types::{Height, Outpoint, TransparentSpend};
use zaino_service::error::SpendReadError;
use zaino_service::{Capability, SpendRead, SpendStatus};

use crate::spend_resolve::{resolve_spend, ResolveError};
use crate::{read_index_value, read_keyed, serviceability_gate, StoreSnapshot};

/// The store answers spend status wherever its index set builds the three
/// indexes the capability composes from.
impl<B, M> SpendRead for StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Backs<local::SpendStatus>,
{
    async fn spend_status(&self, outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        let backend = self.backend.clone();
        let reader = backend
            .reader()
            .map_err(|e| SpendReadError::Fatal(format!("open reader: {e}")))?;
        spend_status_gate::<B>(&reader)?;

        let key = OutpointKey {
            prev_txid: outpoint.txid,
            prev_index: outpoint.index,
        };
        if let Some(by) = read_spender(&reader, &key)
            .map_err(|e| SpendReadError::Transient(format!("read transparent_spends: {e}")))?
        {
            return Ok(SpendStatus::Spent { by });
        }

        // No spend recorded. Whether that means unspent or never-created is the
        // output's existence, resolved through the txid's location.
        if output_exists::<B>(&reader, outpoint)? {
            Ok(SpendStatus::Unspent)
        } else {
            Ok(SpendStatus::NoSuchOutput)
        }
    }

    async fn spend_info(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TransparentSpend>, SpendReadError> {
        let backend = self.backend.clone();
        let reader = backend
            .reader()
            .map_err(|e| SpendReadError::Fatal(format!("open reader: {e}")))?;
        spend_status_gate::<B>(&reader)?;

        let key = OutpointKey {
            prev_txid: outpoint.txid,
            prev_index: outpoint.index,
        };
        // `None` is "no spend recorded" (unspent or never-created), which this
        // read collapses into the one "not spent here" answer the composer falls
        // through on. The shared resolver's failures fold into this read's error.
        Ok(resolve_spend::<B>(&reader, key)
            .map_err(spend_read_error)?
            .map(|resolved| TransparentSpend {
                outpoint,
                by: resolved.by,
                input_index: resolved.input_index,
                height: resolved.height,
                block_index: resolved.block_index,
            }))
    }
}

/// Refuse a spend read whose backing scattered namespaces are still deferred.
///
/// Spend status is composed from the spends index and the location index (which
/// resolves "no spend recorded" into unspent-vs-never-created). Both are
/// `Scattered` and may still be building during the initial catch-up, so while
/// either is incomplete the read answers `NotServiceable(SpendStatus)` rather
/// than reading an absent spend as unspent. One completeness probe per namespace
/// on the pinned reader, checked once per read.
fn spend_status_gate<B: Backend + 'static>(reader: &B::Reader) -> Result<(), SpendReadError> {
    if let Some(capability) = serviceability_gate::<B>(
        reader,
        &[transparent_spends::ID.into(), txid_location::ID.into()],
        Capability::SpendStatus,
    )
    .map_err(|e| SpendReadError::Transient(format!("spend-status readiness: {e}")))?
    {
        return Err(SpendReadError::NotServiceable(capability));
    }
    Ok(())
}

/// Fold a shared spend-resolution failure into this read's error, preserving its
/// transient/fatal classification.
fn spend_read_error(error: ResolveError) -> SpendReadError {
    match error {
        ResolveError::Transient(message) => SpendReadError::Transient(message),
        ResolveError::Fatal(message) => SpendReadError::Fatal(message),
    }
}

/// Whether the finalised range holds a transaction that created `outpoint`.
///
/// Locates the transaction, then asks that block's transparent data whether it
/// has an output at the index. A located transaction whose block carries no
/// transparent entry for it is index corruption rather than absence — the
/// engine commits both in one batch — so it is reported as fatal rather than
/// read as "no such output".
fn output_exists<B>(reader: &B::Reader, outpoint: Outpoint) -> Result<bool, SpendReadError>
where
    B: Backend + 'static,
{
    let location =
        read_keyed::<TxidLocationIndex, B>(reader, txid_location::ID.into(), &outpoint.txid)
            .map_err(|t| SpendReadError::Transient(t.0))?;
    let Some(location) = location else {
        return Ok(false);
    };

    let height = u32::try_from(location.height.value())
        .ok()
        .and_then(|h| Height::try_from(h).ok())
        .ok_or_else(|| {
            SpendReadError::Fatal("indexed height exceeds the protocol limit".to_owned())
        })?;

    let block =
        read_index_value::<TransparentDataIndex, B>(reader, transparent_data::ID.into(), height)
            .map_err(|t| SpendReadError::Transient(t.0))?
            .ok_or_else(|| {
                SpendReadError::Fatal(format!(
                    "transaction located at height {} but that block has no transparent data",
                    u32::from(height),
                ))
            })?;

    let index = usize::try_from(location.tx_index)
        .map_err(|_| SpendReadError::Fatal("transaction index exceeds usize".to_owned()))?;
    let tx = block.0.get(index).ok_or_else(|| {
        SpendReadError::Fatal(format!(
            "transaction located at position {index} of height {}, which holds {} transactions",
            u32::from(height),
            block.0.len(),
        ))
    })?;

    let output = usize::try_from(outpoint.index)
        .map_err(|_| SpendReadError::Fatal("output index exceeds usize".to_owned()))?;
    Ok(output < tx.outputs.len())
}
