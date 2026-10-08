//! [`Snapshot`]: every answer Zaino serves, from one value (`global-snapshot.md` §2)

use std::sync::Arc;

use zaino_chainview::{ChainViewSnapshot, EndpointSet, MempoolView};
use zaino_header_chain::{Fork, VerifiedChain};
use zaino_nfs::{At, Indexed};
use zaino_persistence::View;
use zaino_primitives::types::{BlockHash, BlockRef, BlockchainInfo, Height};

use crate::feed::{Feed, MempoolTail};

/// - `held_by` = trusted validators holding `best` (V1)
/// - `synced`: opens at served == best, closes off best or > depth behind
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tips {
    pub best: Option<BlockRef>,
    pub final_tip: Option<BlockRef>,
    pub served: Option<BlockRef>,
    pub held_by: EndpointSet,
    pub synced: bool,
}

/// One fork of the chain view's chain + the deepest block of it the NFS folded
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkView {
    pub fork: Fork,
    pub folded: Option<BlockRef>,
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

/// `/readyz` reasons, in order
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unready {
    HeadersSyncing,
    TipNotHeld,
    Syncing,
}

impl Unready {
    /// `/readyz` + `/statusz` text
    pub const fn label(self) -> &'static str {
        match self {
            Self::HeadersSyncing => "headers_syncing",
            Self::TipNotHeld => "tip_not_held",
            Self::Syncing => "syncing",
        }
    }
}

/// One publish: tips, the NFS's latest, the chain view's latest, the mempool epoch at `served`
pub struct Snapshot<V> {
    pub(crate) seq: u64,
    pub(crate) tips: Tips,
    pub(crate) indexed: Option<Arc<Indexed<V>>>,
    pub(crate) view: Arc<ChainViewSnapshot>,
    pub(crate) feed: Feed,
}

impl<V> Snapshot<V> {
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

    /// Gate: a `Live` validator holds the tip (G6)
    pub fn mempool(&self) -> Result<MempoolView<'_>, Unavailable> {
        self.view.mempool().ok_or_else(|| self.unheld())
    }

    /// The epoch at `tips.served`: ends once the served tip moves (G5)
    pub fn mempool_stream(&self) -> Result<MempoolTail, Unavailable> {
        self.mempool()?;
        Ok(self.feed.tail())
    }

    /// `GetLightdInfo`: a holder's `getblockchaininfo` + the served height, one load
    pub fn lightd(&self) -> Result<(&BlockchainInfo, Height), Unavailable> {
        let info = self.view.validator_info().ok_or_else(|| self.unheld())?;
        Ok((info, self.served()?.tip().height))
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

    pub fn unready(&self) -> impl Iterator<Item = Unready> + '_ {
        let chain = self.tips.best.is_some();
        let reasons = [
            (!chain, Unready::HeadersSyncing),
            (chain && self.tips.held_by.is_empty(), Unready::TipNotHeld),
            (!self.tips.synced, Unready::Syncing),
        ];
        reasons.into_iter().filter(|(raised, _)| *raised).map(|(_, reason)| reason)
    }

    /// Each fork of `chain()`, most work first, with the deepest block of it folded
    pub fn forks(&self) -> Vec<ForkView> {
        let Some(chain) = self.chain() else { return Vec::new() };
        let folded = |hash: &BlockHash| self.indexed.as_ref().is_some_and(|i| i.folded(hash));
        let view = |fork: Fork| {
            let branch = chain.branch(&fork.tip.hash);
            ForkView { fork, folded: branch.into_iter().rev().find(|at| folded(&at.hash)) }
        };
        chain.forks().into_iter().map(view).collect()
    }
}

impl<V: View> Snapshot<V> {
    /// Index state as of any folded block or the NFS root ([`Indexed::at`])
    pub fn at(&self, hash: &BlockHash) -> Option<At<V>> {
        self.indexed.as_ref()?.at(hash)
    }
}
