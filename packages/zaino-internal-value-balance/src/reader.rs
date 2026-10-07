//! Typed reads over any `MapRead` view (a snapshot's layered view, the committed store alike)

use zaino_persistence::{MapRead, View};
use zaino_primitives::types::{OutPoint, Zatoshis};

use crate::{decode_value, OUTPUTS, VALUE};

#[derive(Clone)]
pub struct ValueBalanceReader<V> {
    view: V,
}

impl<V: MapRead> ValueBalanceReader<V> {
    pub fn new(view: V) -> Self {
        Self { view }
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }

    /// One answer per outpoint, in order: one `values` probe (cold page faults overlap instead of
    /// queueing)
    pub(crate) fn values(&self, outpoints: &[OutPoint]) -> Vec<Option<Zatoshis>> {
        let keys: Vec<[u8; OutPoint::LEN]> = outpoints.iter().map(OutPoint::encode).collect();
        let keys: Vec<&[u8]> = keys.iter().map(|key| &key[..]).collect();
        let found = self.view.map(OUTPUTS).values(&keys).into_iter();
        found
            .map(|found| {
                let found = found?;
                let bytes: &[u8; VALUE] =
                    found[..].try_into().expect("outputs values: schema width");
                Some(decode_value(bytes).expect("outputs values: in supply when committed"))
            })
            .collect()
    }
}

impl<V: View> std::fmt::Debug for ValueBalanceReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValueBalanceReader").field("tip", &self.view.tip()).finish()
    }
}
