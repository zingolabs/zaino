//! The served value-balance index as chainview's [`UnspentOutputs`] (a peer quorum's prevouts)

use zaino_chainview::UnspentOutputs;
use zaino_persistence::DiskView;
use zaino_primitives::types::OutPoint;
use zaino_snapshot::Snapshots;
use zcash_protocol::value::Zatoshis;

/// One snapshot load per ask: the UTXO set at the served tip (`None` = nothing served yet, or
/// value-balance off)
pub(crate) struct ServedUtxos(pub(crate) Snapshots<DiskView>);

impl UnspentOutputs for ServedUtxos {
    fn values(&self, outpoints: &[OutPoint]) -> Option<Vec<Option<Zatoshis>>> {
        let snapshot = self.0.load();
        let served = snapshot.served().ok()?;
        Some(served.views().value_balance()?.values(outpoints))
    }
}
