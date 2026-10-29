//! [`ChainViewSnapshot`] fixtures for consumers' tests: `fixed`, then `trusted_validator`, `ahead`,
//! `peers_listed`

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use zaino_header_chain::HeaderChainSnapshot;
use zaino_primitives::types::{BlockRef, FinalityDepth};
use zaino_source::BlockchainInfo;
use zaino_traffic::{Health, TrustedValidatorId};
use zcash_protocol::{BlockHash, TxId};

use crate::chain_view_snapshot::Sighting;
use crate::{ChainViewSnapshot, TrustedValidatorMeta, TrustedValidatorSet};

impl ChainViewSnapshot {
    /// Consumers' tests: `chain` as header sync's word, `held_by` holding its best
    ///
    /// - one fresh trusted validator per address
    /// - `ours` served (our relay)
    /// - each holder `Live` + `Agreed`, its `getblockchaininfo` = the best block, no upgrade,
    ///   branch id 0
    pub fn fixed(
        chain: Option<Arc<HeaderChainSnapshot>>,
        held_by: TrustedValidatorSet,
        addresses: &[&str],
        ours: &[(TxId, Bytes)],
    ) -> Self {
        let trusted_validators =
            addresses.iter().map(|at| TrustedValidatorMeta::new((*at).to_owned()));
        let mut view = Self::empty(trusted_validators.collect(), FinalityDepth::CONSENSUS);
        view.chain = chain;
        if let Some(best) = view.best() {
            let info = BlockchainInfo {
                tip: best,
                estimated_height: best.height,
                activations: Vec::new(),
            };
            for at in held_by.positions() {
                if let Some(meta) = view.trusted_validators.get_mut(at) {
                    meta.info = Some(info.clone());
                    meta.health = Health::Live;
                }
            }
        }
        for (txid, raw) in ours {
            view.mempool.insert(*txid, Sighting::relayed(raw.clone()));
        }
        view
    }

    /// Consumers' tests: trusted validator `at`'s health, and its clock estimate `behind` blocks
    /// past its tip (a [`fixed`](Self::fixed) holder's tip = best)
    /// - alarms re-derived as a merge would
    pub fn trusted_validator(mut self, at: usize, health: Health, behind: u32) -> Self {
        if let Some(meta) = self.trusted_validators.get_mut(at) {
            meta.health = health;
            if let Some(info) = meta.info.as_mut() {
                info.estimated_height = info.tip.height + behind;
            }
        }
        let paused = self.alarms.raised(crate::telemetry::Alarm::FinalityPaused);
        self.alarms = crate::telemetry::alarms(&self.trusted_validators, paused);
        self
    }

    /// Consumers' tests: holder `at` claims `by` blocks past best (header sync short of its tip)
    ///
    /// - `read` = its hash at best polled → `Ahead`, still a holder
    /// - `!read` = no poll since the chain moved → `Diverged`, not a holder (as a merge would)
    pub fn ahead(mut self, at: usize, by: u32, read: bool) -> Self {
        let best = self.best().expect("ahead of a header chain best");
        let id = TrustedValidatorId::new(at).expect("a small position");
        assert!(self.held_by().contains(id), "{at}: not a holder");
        let meta =
            self.trusted_validators.get_mut(at).expect("a holder = a configured trusted validator");
        let info = meta.info.as_mut().expect("a holder answered");
        info.tip = BlockRef { hash: BlockHash([0xA0; 32]), height: best.height + by };
        info.estimated_height = info.tip.height;
        meta.block_at_best = read.then_some(best);
        self
    }

    /// Consumers' tests: one fresh, empty mempool listing from each of `peers` (quorate iff they
    /// span > `ECLIPSE_NETGROUPS` netgroups)
    pub fn peers_listed(mut self, peers: &[SocketAddr]) -> Self {
        let answers = peers.iter().map(|peer| (*peer, Some(Vec::new()))).collect();
        self.listings.round(answers, tokio::time::Instant::now());
        self
    }
}
