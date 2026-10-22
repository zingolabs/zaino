//! G2–G5, G10 checker between two consecutive publishes (model + fire drills)
//!
//! - G6 (the mempool gate) = the model's naive gate + the gate table (`tests.rs`): `mempool()`
//!   passes the chain view's gate through, nothing here to restate it from

use zaino_primitives::types::FinalityDepth;

use crate::mempool_feed::above_durable_txids;

/// G2–G5, G10 between two consecutive publishes
///
/// - panics naming the invariant broken
/// - after `next`'s store and `prev`'s seal (the publisher's order)
pub(crate) fn check<V>(
    prev: &crate::GrpcSnapshot<V>,
    next: &crate::GrpcSnapshot<V>,
    depth: FinalityDepth,
) {
    let seq = (prev.seq, next.seq);
    assert_eq!(next.seq, prev.seq + 1, "seq + 1 per publish (G2): (prev, next) seq {seq:?}");
    let tips = next.tips;
    let chain = next.view.chain();
    let best = chain.map(|chain| chain.best());
    assert_eq!(tips.best, best, "tips.best (left) = the view chain's best (right) (G3)");
    let finalized_tip = chain.map(|chain| chain.finalized_tip());
    let message = "tips.finalized_tip (left) = the view chain's finalized tip (right) (G3)";
    assert_eq!(tips.finalized_tip, finalized_tip, "{message}");
    let served = next.indexed.as_ref().map(|indexed| indexed.served().tip());
    assert_eq!(tips.served, served, "tips.served (left) = the NFS's served tip (right) (G3)");

    if tips.synced {
        let chain = chain.expect("synced under a chain (G4)");
        let (best, served) = (chain.best(), tips.served.expect("synced = something served (G4)"));
        let claimed = next.view.claimed_past_best();
        let opened = prev.tips.synced || (served == best && !claimed);
        let (best_height, served_height) = (best.height, served.height);
        assert!(
            opened,
            "synced opens only at served = best, no validator claiming past it (G4): served \
             {served_height:?}, best {best_height:?}, claimed past best {claimed}"
        );
        let message = "synced stays on best (G4)";
        assert!(chain.on_best(served), "{message}: served {served:?} off best {best:?}");
        let behind = u32::from(best.height) - u32::from(served.height);
        let depth = depth.get();
        assert!(
            behind <= depth,
            "synced stays within depth {depth} of best (G4): served {served_height:?}, best \
             {best_height:?} ({behind} behind)"
        );
    }

    let moved = tips.served != prev.tips.served;
    let rotated = !next.feed.same_epoch(&prev.feed);
    let served_moved = format!("served {:?} → {:?}", prev.tips.served, tips.served);
    let message = "epoch rotated (left) iff the served tip moved (right) (G5)";
    assert_eq!(rotated, moved, "{message}: {served_moved}");
    let message = "old epoch sealed (left) iff the served tip moved (right) (G5)";
    assert_eq!(prev.feed.sealed(), moved, "{message}: {served_moved}");
    assert!(!next.feed.sealed(), "stored epoch open (G5): {served_moved}");

    let mined = above_durable_txids(next.indexed.as_deref());
    let carried = mined.iter().find(|txid| next.feed.carries(txid));
    let message = "epoch carries no tx of the served tip's own chain (G10): carried (left)";
    assert_eq!(carried, None, "{message}");
}
