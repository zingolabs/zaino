//! Tips composition: one publish's [`Tips`] from that publish's inputs alone

use crate::grpc_snapshot::Tips;
use zaino_chainview::ChainViewSnapshot;
use zaino_header_chain::HeaderChainSnapshot;
use zaino_nfs::NfsPublication;
use zaino_primitives::types::{BlockRef, FinalityDepth};

/// Pure (no I/O, no clock)
///
/// - `was_synced` = the last publish's (hysteresis)
pub(crate) fn compose<V>(
    was_synced: bool,
    indexed: Option<&NfsPublication<V>>,
    view: &ChainViewSnapshot,
    depth: FinalityDepth,
) -> Tips {
    let chain = view.chain().map(|chain| &**chain);
    let served = indexed.map(|indexed| indexed.served().tip());
    Tips {
        best: chain.map(HeaderChainSnapshot::best),
        finalized_tip: chain.map(HeaderChainSnapshot::finalized_tip),
        served,
        synced: chain.is_some_and(|chain| {
            synced(was_synced, served, chain, depth, view.claimed_past_best())
        }),
    }
}

/// - opens: served = best block (hash, not height) & no validator claiming past it (`ahead`)
/// - stays: served on the best & ≤ `depth` behind it
fn synced(
    was: bool,
    served: Option<BlockRef>,
    chain: &HeaderChainSnapshot,
    depth: FinalityDepth,
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
