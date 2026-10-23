//! Typed reads over any `MapRead` view (an `NfsPublication`'s layered view, the durable store
//! alike)

use zaino_persistence::{MapRead, View};
use zcash_protocol::TxId;

use crate::by_txid::{decode, Mined, BY_TXID, MINED};

#[derive(Clone)]
pub struct TxidLocationReader<V> {
    view: V,
}

impl<V: MapRead> TxidLocationReader<V> {
    pub fn new(view: V) -> Self {
        Self { view }
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }

    pub fn mined(&self, txid: &TxId) -> Option<Mined> {
        let value = self.view.map(BY_TXID).value(txid.as_ref())?;
        let bytes: &[u8; MINED] = value[..].try_into().expect("by_txid values: schema width");
        Some(decode(bytes))
    }
}

impl<V: View> std::fmt::Debug for TxidLocationReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxidLocationReader").field("tip", &self.view.tip()).finish()
    }
}
