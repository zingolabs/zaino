//! Held blocks + committed store, pinned together once per request

use zaino_persistence::{MapRead, TieredView, View};
use zaino_primitives::types::Height;

use crate::{
    by_hash::{decode_height, BY_HASH, HEIGHT},
    HASH,
};

#[derive(Clone)]
pub struct ReadView<V> {
    view: TieredView<V>,
}

impl<V: MapRead> ReadView<V> {
    pub(crate) fn new(view: TieredView<V>) -> Self {
        Self { view }
    }

    pub(crate) fn height_of_hash(&self, hash: &[u8; HASH]) -> Option<Height> {
        let value = self.view.value(BY_HASH, hash)?;
        let bytes: &[u8; HEIGHT] = value[..].try_into().expect("by_hash values: schema width");
        Some(decode_height(bytes).expect("by_hash heights: valid when committed"))
    }
}

impl<V: View> std::fmt::Debug for ReadView<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadView").field("view", &self.view).finish()
    }
}
