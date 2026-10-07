//! Typed reads over any `MapRead` view (held tiers, a layer, the committed store alike)

use zaino_persistence::{MapRead, View};
use zaino_primitives::types::{BlockHash, Height};

use crate::{
    by_hash::{decode_height, BY_HASH, HEIGHT},
    HASH,
};

#[derive(Clone)]
pub struct BlockHashReader<V> {
    view: V,
}

impl<V: MapRead> BlockHashReader<V> {
    pub fn new(view: V) -> Self {
        Self { view }
    }

    pub fn height_of(&self, hash: &BlockHash) -> Option<Height> {
        let value = self.view.value(BY_HASH, &<[u8; HASH]>::from(*hash))?;
        let bytes: &[u8; HEIGHT] = value[..].try_into().expect("by_hash values: schema width");
        Some(decode_height(bytes).expect("by_hash heights: valid when committed"))
    }
}

impl<V: View> std::fmt::Debug for BlockHashReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockHashReader").field("tip", &self.view.tip()).finish()
    }
}
