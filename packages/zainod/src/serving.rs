//! Whether the NFS serves the verified tip: `/readyz`, `zaino.index.synced`, the index reports
//!
//! - Opens once the served tip **is** the verified best (hash, not height)
//! - Closes once it leaves the best chain or falls more than `depth` behind it (a stalled NFS)
//! - Requests never wait on it: every request answers at its snapshot's tip

use std::sync::Arc;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::info;
use zaino_header_chain::VerifiedChain;
use zaino_nfs::NfsHandle;
use zaino_persistence::DiskView;
use zaino_primitives::types::{BlockRef, ReorgDepth};

use crate::error::IndexerError;

/// `synced` kept current until `cancel`
pub(crate) async fn run(
    mut snapshots: NfsHandle<DiskView>,
    mut verified: watch::Receiver<Option<Arc<VerifiedChain>>>,
    depth: ReorgDepth,
    synced: watch::Sender<bool>,
    cancel: CancellationToken,
) -> Result<(), IndexerError> {
    loop {
        let served = snapshots.snapshot().map(|snap| snap.tip());
        let chain = verified.borrow_and_update().clone();
        let was = *synced.borrow();
        let now = chain.is_some_and(|chain| at_tip(was, served, &chain, depth));
        if synced.send_replace(now) != now {
            let height = served.map_or(0, |tip| u32::from(tip.height));
            match now {
                true => info!(height, "Serving the verified tip"),
                false => info!(height, "Behind the verified tip, syncing"),
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            moved = snapshots.changed() => if moved.is_err() { return gone(&cancel).await },
            moved = verified.changed() => if moved.is_err() { return gone(&cancel).await },
        }
    }
}

/// An input gone (its task ended: the supervisor reports it): nothing left to judge
async fn gone(cancel: &CancellationToken) -> Result<(), IndexerError> {
    cancel.cancelled().await;
    Ok(())
}

/// `was` = the last answer (hysteresis: open at the best, closed past `depth` behind it)
fn at_tip(was: bool, served: Option<BlockRef>, chain: &VerifiedChain, depth: ReorgDepth) -> bool {
    let Some(served) = served else { return false };
    let best = chain.best();
    let on_best = chain.hash_at(served.height) == Some(served.hash);
    let behind = u32::from(best.height).saturating_sub(served.height.into());
    match was {
        false => served == best,
        true => on_best && behind <= depth.get(),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::Block;

    use super::*;

    /// Depth 3, chain A 0..=24, B22 forking after A21: closed below the best, open at it, open
    /// within the depth, closed past it (a stalled NFS), closed once the best moves off the
    /// served block's branch, reopened at the new best
    #[test]
    fn open_at_the_best_block_closed_past_the_depth_or_off_the_best_branch() {
        let mut chain = Chain::new();
        let a24 = chain.extend(chain.genesis().hash, 24);
        let a: Vec<Block> = chain.path(a24.hash);
        let b22 = chain.mine_bits(a[21].header().hash, a[22].header().time, 0x1f0f_0f0f);
        let at =
            |block: &Block| BlockRef { hash: block.header().hash, height: block.header().height };
        let best_a = |h: usize| VerifiedChain::regtest(&a[..=h]);
        let best_b = VerifiedChain::regtest(&chain.path(b22.hash));
        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("non-zero"));

        let steps = [
            ("served 10, best 20", false, Some(at(&a[10])), best_a(20), false),
            ("nothing served", false, None, best_a(20), false),
            ("served = best", false, Some(at(&a[20])), best_a(20), true),
            ("3 behind = within the depth", true, Some(at(&a[20])), best_a(23), true),
            ("4 behind = stalled", true, Some(at(&a[20])), best_a(24), false),
            ("A22 off B's best", true, Some(at(&a[22])), best_b.clone(), false),
            (
                "A21 on B's best, 1 behind, was closed",
                false,
                Some(at(&a[21])),
                best_b.clone(),
                false,
            ),
            ("B22 = best", false, Some(at(chain.block(b22.hash))), best_b, true),
        ];
        for (case, was, served, chain, expected) in steps {
            assert_eq!(at_tip(was, served, &chain, depth), expected, "{case}");
        }
    }
}
