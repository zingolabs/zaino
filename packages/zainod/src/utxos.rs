//! The served utxo-set index as chainview's [`UnspentOutputs`] (a peer quorum's prevouts)

use zaino_chainview::UnspentOutputs;
use zaino_grpc_snapshot::GrpcSnapshots;
use zaino_persistence::DiskView;
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

/// One `GrpcSnapshot` load per ask: the UTXO set at the served tip (`None` = nothing served yet, or
/// utxo-set off)
pub(crate) struct ServedUtxos(pub(crate) GrpcSnapshots<DiskView>);

impl UnspentOutputs for ServedUtxos {
    fn values(&self, outpoints: &[OutPoint]) -> Option<Vec<Option<Zatoshis>>> {
        let grpc_snapshot = self.0.load();
        let served = grpc_snapshot.served().ok()?;
        Some(served.views().utxo_set()?.values(outpoints))
    }
}
