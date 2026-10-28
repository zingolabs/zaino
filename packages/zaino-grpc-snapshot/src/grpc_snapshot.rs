//! [`GrpcSnapshot`]: every answer Zaino serves, from one value (`usage.md`)

use std::sync::Arc;

use zaino_chainview::{ChainViewSnapshot, MempoolView, STALE_TIP_BLOCKS};
use zaino_header_chain::HeaderChainSnapshot;
use zaino_nfs::{IndexesAtBlock, NfsPublication};
use zaino_primitives::types::BlockRef;
use zaino_traffic::Health;
use zcash_protocol::consensus::{target_spacing, BlockHeight};
use zcash_protocol::local_consensus::LocalNetwork;

use crate::mempool_feed::{Feed, MempoolTail};

/// Best blocks past the last vouched one before no trusted validator vouches for the tip (a peer's
/// block one poll ahead of every validator = the race a poll closes)
const UNVOUCHED_BLOCKS: u32 = 1;

/// - `synced`: opens at served == best, closes off best or > depth behind
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tips {
    pub best: Option<BlockRef>,
    pub finalized_tip: Option<BlockRef>,
    pub served: Option<BlockRef>,
    pub synced: bool,
}

/// What `GetLightdInfo` renders (`GrpcSnapshot::lightd`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lightd {
    pub served: BlockHeight,
    pub best: BlockHeight,
    pub schedule: LocalNetwork,
}

/// No answer (fail closed: `UNAVAILABLE`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    #[error("no verified header chain tip yet")]
    NoChain,
    #[error("no trusted validator holds the verified tip {height} (of {configured} configured)")]
    NotHeld { height: u32, configured: usize },
    #[error("the indexes are syncing: nothing served yet")]
    NothingServed,
}

/// Serving judged from one snapshot (zainod adds held, provisioning, a stalled runtime)
///
/// - `Degraded` = serving a fresh best no trusted validator vouches for (peers carried it)
/// - `Down` = that best stale or not yet served
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServingStatus {
    Serving,
    Degraded,
    Syncing,
    Down,
}

/// One publish: tips, the NFS's latest, the chain view's latest, the mempool epoch at `served`
pub struct GrpcSnapshot<V> {
    pub(crate) seq: u64,
    pub(crate) tips: Tips,
    pub(crate) indexed: Option<Arc<NfsPublication<V>>>,
    pub(crate) view: Arc<ChainViewSnapshot>,
    pub(crate) feed: Feed,
}

impl<V> GrpcSnapshot<V> {
    pub fn tips(&self) -> Tips {
        self.tips
    }

    /// The chain view's: `tips` judged under it
    fn chain(&self) -> Option<&Arc<HeaderChainSnapshot>> {
        self.view.chain()
    }

    /// Index state at the served tip (block, tree-state and transparent routes)
    pub fn served(&self) -> Result<&IndexesAtBlock<V>, Unavailable> {
        let indexed = self.indexed.as_deref().ok_or(Unavailable::NothingServed)?;
        Ok(indexed.served())
    }

    /// Gate: a `Live` validator holds the tip, or a peer quorum reads (G6)
    pub fn mempool(&self) -> Result<MempoolView<'_>, Unavailable> {
        self.view.mempool().ok_or_else(|| self.unheld())
    }

    /// The epoch at `tips.served`: ends once the served tip moves (G5)
    pub fn mempool_stream(&self) -> Result<MempoolTail, Unavailable> {
        self.mempool()?;
        Ok(self.feed.tail())
    }

    /// `GetLightdInfo`: the served height, the verified best (the network tip estimate), the
    /// schedule they read, one load (no validator: served degraded too)
    pub fn lightd(&self) -> Result<Lightd, Unavailable> {
        let served = self.served()?;
        let best = self.tips.best.map_or(served.tip().height, |best| best.height);
        Ok(Lightd { served: served.tip().height, best, schedule: served.params().schedule })
    }

    /// Why no trusted validator vouches for the best block
    fn unheld(&self) -> Unavailable {
        match self.tips.best {
            None => Unavailable::NoChain,
            Some(best) => Unavailable::NotHeld {
                height: u32::from(best.height),
                configured: self.view.trusted_validators().len(),
            },
        }
    }

    /// Validator facts, alarms, the mempool's spread
    pub fn chain_view(&self) -> &ChainViewSnapshot {
        &self.view
    }

    /// Memo identity: a new `Arc` per NFS publish
    pub fn indexed(&self) -> Option<&Arc<NfsPublication<V>>> {
        self.indexed.as_ref()
    }

    /// First match (`now_unix` = the clock):
    ///
    /// - best more than `UNVOUCHED_BLOCKS` past the last vouched block, or every trusted
    ///   validator down under a stale best (`[p2p]` off: nothing moves the tip) → `Degraded` while
    ///   synced to a fresh best, else `Down`
    /// - not synced, or a validator's mempool off and none on (zebrad's, for minutes after a
    ///   catch-up) → `Syncing`
    /// - else `Serving`
    pub fn serving_status(&self, now_unix: i64) -> ServingStatus {
        let trusted_validators = self.view.trusted_validators();
        let health = |wanted| trusted_validators.iter().any(|m| m.health == wanted);
        let all_down = !trusted_validators.is_empty()
            && trusted_validators.iter().all(|m| m.health == Health::Down);
        let vouched = self.chain().is_none_or(|chain| {
            chain.vouched(chain.best().height.saturating_sub(UNVOUCHED_BLOCKS))
        });
        if !vouched || (all_down && !self.fresh(now_unix)) {
            return match self.tips.synced && self.fresh(now_unix) {
                true => ServingStatus::Degraded,
                false => ServingStatus::Down,
            };
        }
        let mempool_off = health(Health::CatchingUp) && !health(Health::Live);
        match self.tips.synced && !mempool_off {
            true => ServingStatus::Serving,
            false => ServingStatus::Syncing,
        }
    }

    /// Best block's time within [`STALE_TIP_BLOCKS`] target spacings of `now_unix` (nothing served
    /// = not fresh)
    fn fresh(&self, now_unix: i64) -> bool {
        let (Some(best), Ok(served)) = (self.tips.best, self.served()) else { return false };
        let Some(header) = self.chain().and_then(|chain| chain.header_at(best.height)) else {
            return false;
        };
        let spacing = target_spacing(&served.params().schedule, best.height);
        now_unix - i64::from(header.time) < i64::from(STALE_TIP_BLOCKS * spacing)
    }

    /// Best's height, or above it the clock estimate of a validator still catching up
    pub fn sync_target(&self) -> Option<BlockHeight> {
        let best = self.tips.best.map(|best| best.height);
        let catching_up =
            self.view.trusted_validators().iter().filter(|meta| meta.health == Health::CatchingUp);
        let estimates =
            catching_up.filter_map(|meta| Some(meta.tip()?.height + meta.stale_blocks()?));
        best.into_iter().chain(estimates).max()
    }
}
