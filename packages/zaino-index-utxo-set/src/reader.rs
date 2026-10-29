//! Typed reads over any view (an `NfsPublication`'s layered view, the durable store alike)

use zaino_persistence::{MapRead, SequenceRead, View};
use zaino_primitives::types::{Block, BlockFees};
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

use crate::layout::{decode_fees, decode_value, encode_outpoint, OUTPOINT, VALUE};
use crate::{FEES, OUTPUTS};

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

    /// `block`'s fees as folded when this view took it in
    ///
    /// - callers: `block` = the block this view holds at its height (else a panic)
    pub fn block_fees(&self, block: &Block) -> BlockFees {
        let header = block.header();
        let height = header.height;
        let record = self.view.sequence(FEES).record(u64::from(height));
        let record = record.unwrap_or_else(|| panic!("utxo_set: fees record at held {height}"));
        let fees = decode_fees(&record)
            .unwrap_or_else(|| panic!("utxo_set: fees record at {height} decodes (encode_fees)"));
        let block_fees = BlockFees { height, hash: header.hash, fees };
        assert!(
            block_fees.belongs_to(block),
            "utxo_set: fees record at {height} = the held block's (one per tx)"
        );
        block_fees
    }
}

impl<V: View> std::fmt::Debug for UtxoSetReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UtxoSetReader").field("tip", &self.view.tip()).finish()
    }
}
