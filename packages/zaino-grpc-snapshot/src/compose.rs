//! [`compose`]: one publish's tips from that publish's inputs alone; [`check`]: G2–G6, G10

use crate::grpc_snapshot::Tips;
use zaino_chainview::ChainViewSnapshot;
use zaino_header_chain::VerifiedChain;
use zaino_nfs::Indexed;
use zaino_primitives::types::{BlockRef, ReorgDepth};

/// Pure (no I/O, no clock); `was_synced` = the last publish's (hysteresis)
pub(crate) fn compose<V>(
    was_synced: bool,
    indexed: Option<&Indexed<V>>,
    view: &ChainViewSnapshot,
    depth: ReorgDepth,
) -> Tips {
    let chain = view.chain().map(|chain| &**chain);
    let served = indexed.map(|indexed| indexed.served().tip());
    Tips {
        best: chain.map(VerifiedChain::best),
        finalized_tip: chain.map(VerifiedChain::finalized_tip),
        served,
        held_by: view.held_by(),
        synced: chain.is_some_and(|chain| {
            synced(was_synced, served, chain, depth, view.claimed_past_best())
        }),
    }
}

/// Opens at the best block (hash, not height), no validator claiming past it (`ahead`); stays
/// while on the best and ≤ `depth` behind it
fn synced(
    was: bool,
    served: Option<BlockRef>,
    chain: &VerifiedChain,
    depth: ReorgDepth,
    ahead: bool,
) -> bool {
    let Some(served) = served else { return false };
    let best = chain.best();
    let on_best = chain.on_best(served);
    let behind = u32::from(best.height).saturating_sub(served.height.into());
    match was {
        false => served == best && !ahead,
        true => on_best && behind <= depth.get(),
    }
}

/// G2–G6 between two consecutive publishes; panics naming the invariant broken
///
/// - after `next`'s store and `prev`'s seal (the publisher's order)
#[cfg(test)]
pub(crate) fn check<V>(
    prev: &crate::GrpcSnapshot<V>,
    next: &crate::GrpcSnapshot<V>,
    depth: ReorgDepth,
) {
    use crate::feed::above_durable_txids;

    assert_eq!(next.seq, prev.seq + 1, "G2: seq + 1 per publish");
    let tips = next.tips;
    let chain = next.view.chain();
    assert_eq!(tips.best, chain.map(|chain| chain.best()), "G3: best = the view chain's");
    let finalized_tip = chain.map(|chain| chain.finalized_tip());
    assert_eq!(tips.finalized_tip, finalized_tip, "G3: finalized = the view chain's");
    let held_by = next.view.held_by();
    assert_eq!(tips.held_by, held_by, "G3: held_by = the view's holders of best");
    let served = next.indexed.as_ref().map(|indexed| indexed.served().tip());
    assert_eq!(tips.served, served, "G3: served = the NFS's");

    if tips.synced {
        let chain = chain.expect("G4: synced under a chain");
        let (best, served) = (chain.best(), tips.served.expect("G4: synced = something served"));
        let opened = prev.tips.synced || (served == best && !next.view.claimed_past_best());
        assert!(opened, "G4: synced opens only at served = best, no validator claiming past it");
        assert!(chain.on_best(served), "G4: synced stays on best");
        let behind = u32::from(best.height) - u32::from(served.height);
        assert!(behind <= depth.get(), "G4: synced stays within {depth:?} of best");
    }

    let moved = tips.served != prev.tips.served;
    let rotated = !next.feed.same_epoch(&prev.feed);
    assert_eq!(rotated, moved, "G5: epoch rotates iff the served tip moved");
    assert_eq!(prev.feed.sealed(), moved, "G5: the old epoch sealed iff rotated");
    assert!(!next.feed.sealed(), "G5: the stored epoch open");

    let gate = next.mempool().is_ok();
    let reads = mempool_read(next);
    assert_eq!(gate, reads, "G6: mempool() Ok iff a `Live` holder of the tip or a peer quorum");

    let mined = above_durable_txids(next.indexed.as_deref());
    let carried = mined.iter().find(|txid| next.feed.carries(txid));
    assert_eq!(carried, None, "G10: the epoch carries a tx of the served tip's own chain");
}

/// A mempool is read: a `Live` validator (its mempool listed) among the tip's holders, or the
/// peers' listings spanning a quorum's netgroups
#[cfg(test)]
pub(crate) fn mempool_read<V>(grpc_snapshot: &crate::GrpcSnapshot<V>) -> bool {
    use zaino_traffic::Health;

    let trusted_validators = grpc_snapshot.view().trusted_validators();
    let live =
        |at: usize| trusted_validators.get(at).is_some_and(|meta| meta.health == Health::Live);
    grpc_snapshot.tips().held_by.positions().any(live) || grpc_snapshot.view().peers_quorate()
}
