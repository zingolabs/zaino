//! Typed reads over any `MapRead` view (a snapshot's layered view, the committed store alike)

use zaino_persistence::{MapRead, View};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

use crate::by_hash::{decode_height, BY_HASH, HEIGHT};

#[derive(Clone)]
pub struct BlockHashReader<V> {
    view: V,
}

impl<V: MapRead> BlockHashReader<V> {
    pub fn new(view: V) -> Self {
        Self { view }
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }

    pub fn height_of(&self, hash: &BlockHash) -> Option<BlockHeight> {
        let value = self.view.map(BY_HASH).value(&hash.0)?;
        let bytes: &[u8; HEIGHT] = value[..].try_into().expect("by_hash values: schema width");
        Some(decode_height(bytes))
    }
}

impl<V: View> std::fmt::Debug for BlockHashReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockHashReader").field("tip", &self.view.tip()).finish()
    }
}
