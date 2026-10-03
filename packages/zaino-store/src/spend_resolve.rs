//! Resolving a recorded spend to where it happened.
//!
//! The spends index records only which transaction spent an outpoint. Both the
//! address read (a spend as a balance delta, at its location) and the spend read
//! (`getspentinfo`, the spend's coordinates) need more: the spender's height and
//! position, and which of its inputs consumed the outpoint. That is the same
//! three-index walk — spends → txid-location → transparent-data — so it lives
//! here once, and each caller folds the result into its own typed error.

use zaino_indexes::indexes::transparent_data::{self, TransparentDataIndex};
use zaino_indexes::indexes::transparent_spends::{read_spender, OutpointKey};
use zaino_indexes::indexes::txid_location::{self, TxLocation, TxidLocationIndex};
use zaino_persistence::Backend;
use zaino_primitives::types::{Height, OutputIndex, TransactionId};

use crate::{read_index_value, read_keyed};

/// A read-boundary failure, classified the way both reads classify theirs: a
/// backend read that failed is [`Transient`](Self::Transient), index corruption
/// is [`Fatal`](Self::Fatal). Each caller maps it to its own error at the `?`
/// site, so neither read loses its error granularity to the shared helper.
pub(crate) enum ResolveError {
    /// A backend read failed; likely to resolve on retry.
    Transient(String),
    /// Index corruption — an unrecoverable inconsistency.
    Fatal(String),
}

/// A recorded spend resolved to where it happened.
pub(crate) struct ResolvedSpend {
    /// The transaction that consumed the outpoint.
    pub(crate) by: TransactionId,
    /// Height that mined the consuming transaction.
    pub(crate) height: Height,
    /// Position of that transaction within its block.
    pub(crate) block_index: u32,
    /// Position of the consuming input within that transaction.
    pub(crate) input_index: OutputIndex,
}

/// Resolve the spend of `outpoint` recorded at or below the watermark, if any.
///
/// `Ok(None)` is "no spend recorded here" (unspent or never-created). A recorded
/// spend whose transaction cannot be located, or whose block carries no
/// transparent entry naming the outpoint, is index corruption rather than
/// absence — the engine commits the spends and location indexes in one batch —
/// so it is [`ResolveError::Fatal`], never read as absence.
pub(crate) fn resolve_spend<B>(
    reader: &B::Reader,
    outpoint: OutpointKey,
) -> Result<Option<ResolvedSpend>, ResolveError>
where
    B: Backend + 'static,
{
    let Some(by) = read_spender(reader, &outpoint)
        .map_err(|e| ResolveError::Transient(format!("read transparent_spends: {e}")))?
    else {
        return Ok(None);
    };
    let location = read_keyed::<TxidLocationIndex, B>(reader, txid_location::ID.into(), &by)
        .map_err(|t| ResolveError::Transient(t.0))?
        .ok_or_else(|| {
            ResolveError::Fatal("a recorded spend's transaction has no location".to_owned())
        })?;
    let height = domain_height(location.height)?;
    let input_index = input_index::<B>(reader, &location, height, outpoint)?;
    Ok(Some(ResolvedSpend {
        by,
        height,
        block_index: location.tx_index,
        input_index,
    }))
}

/// The on-disk block height as a protocol [`Height`].
fn domain_height(height: zaino_sync::primitives::BlockHeight) -> Result<Height, ResolveError> {
    u32::try_from(height.value())
        .ok()
        .and_then(|h| Height::try_from(h).ok())
        .ok_or_else(|| {
            ResolveError::Fatal("an indexed height exceeds the protocol limit".to_owned())
        })
}

/// Which input of the transaction at `location` consumed `outpoint`, recovered by
/// scanning its transparent inputs for the one naming the outpoint.
fn input_index<B>(
    reader: &B::Reader,
    location: &TxLocation,
    height: Height,
    outpoint: OutpointKey,
) -> Result<OutputIndex, ResolveError>
where
    B: Backend + 'static,
{
    let block =
        read_index_value::<TransparentDataIndex, B>(reader, transparent_data::ID.into(), height)
            .map_err(|t| ResolveError::Transient(t.0))?
            .ok_or_else(|| {
                ResolveError::Fatal(
                    "a spending transaction's block has no transparent data".to_owned(),
                )
            })?;
    let index = usize::try_from(location.tx_index)
        .map_err(|_| ResolveError::Fatal("a tx index exceeds usize".to_owned()))?;
    let tx = block.0.get(index).ok_or_else(|| {
        ResolveError::Fatal("a spending transaction is past the end of its block".to_owned())
    })?;
    let position = tx
        .inputs
        .iter()
        .position(|(prev_txid, prev_index)| {
            *prev_txid == outpoint.prev_txid && *prev_index == outpoint.prev_index
        })
        .ok_or_else(|| {
            ResolveError::Fatal("a recorded spender does not consume the outpoint".to_owned())
        })?;
    OutputIndex::try_from(position)
        .map_err(|_| ResolveError::Fatal("an input index exceeds the wire limit".to_owned()))
}
