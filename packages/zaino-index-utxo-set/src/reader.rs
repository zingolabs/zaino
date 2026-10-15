//! Typed reads over any view (an `NfsPublication`'s layered view, the durable store alike)

use zaino_persistence::{MapRead, SequenceRead, View};
use zaino_primitives::types::{Block, BlockFees};
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

use crate::{
    decode_fees, decode_value, encode_outpoint, FoldError, FEES, OUTPOINT, OUTPUTS, VALUE,
};

#[derive(Clone)]
pub struct UtxoSetReader<V> {
    view: V,
}

impl<V: SequenceRead + MapRead> UtxoSetReader<V> {
    pub fn new(view: V) -> Self {
        Self { view }
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }

    /// One answer per outpoint, in order (`None` = never created or spent): one `values` probe
    /// (cold page faults overlap instead of queueing)
    pub fn values(&self, outpoints: &[OutPoint]) -> Vec<Option<Zatoshis>> {
        let keys: Vec<[u8; OUTPOINT]> = outpoints.iter().map(encode_outpoint).collect();
        let keys: Vec<&[u8]> = keys.iter().map(|key| &key[..]).collect();
        let found = self.view.map(OUTPUTS).values(&keys).into_iter();
        found
            .map(|found| {
                let found = found?;
                let bytes: &[u8; VALUE] =
                    found[..].try_into().expect("outputs values: schema width");
                Some(decode_value(bytes).expect("outputs values: in supply when made durable"))
            })
            .collect()
    }

    /// `block`'s fees as folded when this view took it in (`block` at or below the view's tip)
    pub fn block_fees(&self, block: &Block) -> Result<BlockFees, FoldError> {
        let header = block.header();
        let height = header.height;
        let record = self
            .view
            .sequence(FEES)
            .record(u64::from(u32::from(height)))
            .ok_or(FoldError::StoredFeesMissing { height })?;
        let fees = decode_fees(&record).ok_or(FoldError::StoredFeesUnreadable { height })?;
        let block_fees = BlockFees { height, hash: header.hash, fees };
        match block_fees.belongs_to(block) {
            true => Ok(block_fees),
            false => Err(FoldError::StoredFeesUnreadable { height }),
        }
    }
}

impl<V: View> std::fmt::Debug for UtxoSetReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UtxoSetReader").field("tip", &self.view.tip()).finish()
    }
}
