//! [`GrpcSnapshot`]: every answer Zaino serves, from one value (`usage.md`)

use std::sync::Arc;

use zaino_chainview::{ChainViewSnapshot, MempoolView, TrustedValidatorSet};
use zaino_header_chain::VerifiedChain;
use zaino_nfs::{At, Indexed};
use zaino_persistence::View;
use zaino_primitives::types::BlockRef;
use zaino_traffic::Health;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::BlockHash;

use crate::feed::{Feed, MempoolTail};

/// - `held_by` = trusted validators holding `best`
/// - `synced`: opens at served == best, closes off best or > depth behind
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tips {
    pub best: Option<BlockRef>,
    pub final_tip: Option<BlockRef>,
    pub served: Option<BlockRef>,
    pub held_by: TrustedValidatorSet,
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

/// Serving judged from one snapshot (zainod adds draining, a stalled runtime, provisioning)
///
/// - `Syncing` also while every holder of best is catching up (best trails the network)
/// - `Degraded` = serving a fresh proof-of-work-verified best no trusted validator vouches for
///   (peers'); `Down` = that best stale or not yet served
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Serving,
    Degraded(Down),
    Syncing,
    Down(Down),
}

/// Why no trusted validator vouches for the best block
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Down {
    NoValidatorReachable,
    TipNotHeld,
}

/// Best block time this far behind the clock = stale (24 spacings: P(natural gap) ≈ e^-24)
const STALE_TIP_SECS: i64 = 24 * 75;

impl Down {
    /// `/readyz` + `/status` `detail`
    pub const fn detail(self) -> &'static str {
        match self {
            Self::NoValidatorReachable => "No trusted validator reachable",
            Self::TipNotHeld => "No trusted validator holds the verified tip",
        }
    }
}

/// One publish: tips, the NFS's latest, the chain view's latest, the mempool epoch at `served`
pub struct GrpcSnapshot<V> {
    pub(crate) seq: u64,
    pub(crate) tips: Tips,
    pub(crate) indexed: Option<Arc<Indexed<V>>>,
    pub(crate) view: Arc<ChainViewSnapshot>,
    pub(crate) feed: Feed,
}

impl<V> GrpcSnapshot<V> {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn tips(&self) -> Tips {
        self.tips
    }

    /// The chain view's: `tips` judged under it
    pub fn chain(&self) -> Option<&Arc<VerifiedChain>> {
        self.view.chain()
    }

    /// Index state at the served tip (block, tree-state and transparent routes)
    pub fn served(&self) -> Result<&At<V>, Unavailable> {
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
                configured: self.view.endpoints().len(),
            },
        }
    }

    /// Validator facts, alarms, the mempool's spread
    pub fn view(&self) -> &ChainViewSnapshot {
        &self.view
    }

    /// Memo identity: a new `Arc` per NFS publish
    pub fn indexed(&self) -> Option<&Arc<Indexed<V>>> {
        self.indexed.as_ref()
    }

    /// First match: every validator down, or best unheld → degraded while synced to a fresh best
    /// (`now_unix` = the clock), else down; not synced or best held only by validators catching
    /// up → syncing; else serving
    pub fn standing(&self, now_unix: i64) -> Standing {
        let endpoints = self.view.endpoints();
        let health = |at: usize| endpoints.get(at).map(|meta| meta.health);
        let unvouched =
            if !endpoints.is_empty() && endpoints.iter().all(|meta| meta.health == Health::Down) {
                Some(Down::NoValidatorReachable)
            } else if self.tips.best.is_some() && self.tips.held_by.is_empty() {
                Some(Down::TipNotHeld)
            } else {
                None
            };
        if let Some(why) = unvouched {
            return match self.tips.synced && self.fresh(now_unix) {
                true => Standing::Degraded(why),
                false => Standing::Down(why),
            };
        }
        let holders = || self.tips.held_by.positions().filter_map(health);
        let catching_up = holders().any(|health| health == Health::CatchingUp)
            && !holders().any(|health| health == Health::Live);
        match self.tips.synced && !catching_up {
            true => Standing::Serving,
            false => Standing::Syncing,
        }
    }

    /// Best block's time within [`STALE_TIP_SECS`] of `now_unix`
    fn fresh(&self, now_unix: i64) -> bool {
        let best = self.tips.best.and_then(|best| self.chain()?.header_at(best.height));
        best.is_some_and(|best| now_unix - i64::from(best.time) < STALE_TIP_SECS)
    }

    /// Best's height, or above it the clock estimate of a validator still catching up
    pub fn sync_target(&self) -> Option<BlockHeight> {
        let best = self.tips.best.map(|best| best.height);
        let catching_up =
            self.view.endpoints().iter().filter(|meta| meta.health == Health::CatchingUp);
        let estimates =
            catching_up.filter_map(|meta| Some(meta.tip()?.height + meta.stale_blocks()?));
        best.into_iter().chain(estimates).max()
    }
}

impl<V: View> GrpcSnapshot<V> {
    /// Index state as of any folded block or the NFS root ([`Indexed::at`])
    pub fn at(&self, hash: &BlockHash) -> Option<At<V>> {
        self.indexed.as_ref()?.at(hash)
    }
}
