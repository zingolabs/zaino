//! [`Ledger`]: one tip's spendable state, the M3/M4/M6/M7 checks both mining and the mempool run

use std::collections::{HashMap, HashSet};

use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;
use zcash_transparent::bundle::OutPoint;

use crate::types::{Fee, Nullifier, Transaction};
use zcash_protocol::ShieldedPool;

/// What the next block on one tip may spend and reveal (`MockChain::ledger`)
#[derive(Debug, Clone)]
pub struct Ledger {
    upgrades: LocalNetwork,
    next: BlockHeight,
    unspent: HashMap<OutPoint, Zatoshis>,
    txids: HashSet<TxId>,
    nullifiers: HashSet<(ShieldedPool, Nullifier)>,
    /// ZIP 209 chain value pools: sprout, sapling, orchard, ironwood
    pools: [i64; 4],
}

/// One mempool entry: real bytes, the fee they leave on its tip
#[derive(Debug, Clone)]
pub struct Unmined {
    pub tx: Transaction,
    pub bytes: Vec<u8>,
    pub fee: Zatoshis,
}

/// Why a transaction cannot be mined next on a [`Ledger`]'s tip
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("txid repeated on this branch")]
    Repeated,
    #[error("pool data before {0}")]
    Inactive(NetworkUpgrade),
    #[error("{0:?} nullifier revealed twice on this branch")]
    Revealed(ShieldedPool),
    #[error("a coinbase spends nothing")]
    CoinbaseSpends,
    #[error("a coinbase pays no fee")]
    CoinbaseFee,
    #[error("a coinbase is never unmined")]
    Coinbase,
    #[error("spends {}:{}, not unspent on this branch", .0.txid(), .0.n())]
    Unspent(OutPoint),
    #[error("takes {0} from a pool holding less (ZIP 209)")]
    Drained(i64),
    #[error("overspends by {0}")]
    Overspends(i64),
    #[error("inputs or outputs summing past the supply")]
    PastSupply,
    #[error("stated fee {stated} != inputs − outputs + balances ({derived})")]
    StatedFee { stated: u64, derived: u64 },
}

impl Ledger {
    /// Nothing mined; the next block at `next`
    pub(super) fn empty(upgrades: LocalNetwork, next: BlockHeight) -> Self {
        Self {
            upgrades,
            next,
            unspent: HashMap::new(),
            txids: HashSet::new(),
            nullifiers: HashSet::new(),
            pools: [0; 4],
        }
    }

    /// `tx` spent here as an unmined (non-coinbase) transaction; its fee, or nothing changed
    pub fn admit(&mut self, tx: &Transaction) -> Result<Zatoshis, Refused> {
        if tx.transparent.coinbase {
            return Err(Refused::Coinbase);
        }
        match self.apply(tx, None)? {
            Fee::Paid(fee) => Ok(fee),
            Fee::Coinbase => Err(Refused::Coinbase),
        }
    }

    /// Each admitted in order, refused ones dropped (a mempool after its tip moved)
    pub fn readmit(
        &mut self,
        candidates: impl IntoIterator<Item = (Transaction, Vec<u8>)>,
    ) -> Vec<Unmined> {
        let admitted = candidates.into_iter().filter_map(|(tx, bytes)| {
            let fee = self.admit(&tx).ok()?;
            Some(Unmined { tx, bytes, fee })
        });
        admitted.collect()
    }

    /// `None` = never created on this branch, or spent
    pub fn unspent(&self, at: &OutPoint) -> Option<Zatoshis> {
        self.unspent.get(at).copied()
    }

    /// Next block one higher (a mined block's transactions all applied)
    pub(super) fn advance(&mut self) {
        self.next = self.next + 1;
    }

    /// M3 (distinct txids), M4 (coinbase spends nothing), M6 (spends, conservation, fee), M7
    /// (pools) at `next`; all checked before anything held changes
    pub(super) fn apply(&mut self, tx: &Transaction, stated: Option<u64>) -> Result<Fee, Refused> {
        let txid = tx.txid;
        if self.txids.contains(&txid) {
            return Err(Refused::Repeated);
        }
        let balances = tx.pool_balances();
        let pools = [
            (NetworkUpgrade::Sapling, tx.sapling.spends.len() + tx.sapling.outputs.len()),
            (NetworkUpgrade::Nu5, tx.orchard.actions.len()),
            (NetworkUpgrade::Nu6_3, tx.ironwood.actions.len()),
        ];
        for ((upgrade, data), balance) in pools.into_iter().zip(&balances[1..]) {
            let used = data > 0 || *balance != 0;
            if used && !self.upgrades.is_nu_active(upgrade, self.next) {
                return Err(Refused::Inactive(upgrade));
            }
        }
        let mut revealed = HashSet::new();
        for (pool, nullifier) in nullifiers(tx) {
            if self.nullifiers.contains(&(pool, nullifier)) || !revealed.insert((pool, nullifier)) {
                return Err(Refused::Revealed(pool));
            }
        }

        let coinbase = tx.transparent.coinbase;
        let withdraws = balances.iter().any(|balance| *balance > 0);
        let spends = !tx.transparent.inputs.is_empty() || !tx.sapling.spends.is_empty();
        if coinbase && (spends || withdraws) {
            return Err(Refused::CoinbaseSpends);
        }
        if coinbase && stated.is_some() {
            return Err(Refused::CoinbaseFee);
        }

        let mut spent = Zatoshis::ZERO;
        let mut inputs = HashSet::new();
        for prevout in &tx.transparent.inputs {
            let value = self.unspent.get(prevout).filter(|_| inputs.insert(prevout));
            let value = value.ok_or_else(|| Refused::Unspent(prevout.clone()))?;
            spent = (spent + *value).ok_or(Refused::PastSupply)?;
        }
        let mut pools = self.pools;
        for (pool, balance) in pools.iter_mut().zip(balances) {
            *pool -= balance;
            if *pool < 0 {
                return Err(Refused::Drained(balance));
            }
        }
        let fee = match coinbase {
            true => Fee::Coinbase,
            false => {
                let remaining = tx.value_left(spent).ok_or(Refused::PastSupply)?;
                let derived =
                    u64::try_from(remaining).map_err(|_| Refused::Overspends(-remaining))?;
                if let Some(stated) = stated.filter(|stated| *stated != derived) {
                    return Err(Refused::StatedFee { stated, derived });
                }
                Fee::Paid(Zatoshis::from_u64(derived).expect("a fee within the supply"))
            }
        };

        self.txids.insert(txid);
        self.nullifiers.extend(revealed);
        for prevout in &tx.transparent.inputs {
            self.unspent.remove(prevout);
        }
        self.pools = pools;
        for (vout, output) in (0..).zip(&tx.transparent.outputs) {
            self.unspent.insert(OutPoint::new(txid.into(), vout), output.value);
        }
        Ok(fee)
    }
}

fn nullifiers(tx: &Transaction) -> impl Iterator<Item = (ShieldedPool, Nullifier)> + '_ {
    let sapling = tx.sapling.spends.iter().map(|spend| (ShieldedPool::Sapling, spend.nullifier));
    let orchard = tx.orchard.actions.iter().map(|action| (ShieldedPool::Orchard, action.nullifier));
    let ironwood =
        tx.ironwood.actions.iter().map(|action| (ShieldedPool::Ironwood, action.nullifier));
    sapling.chain(orchard).chain(ironwood)
}
