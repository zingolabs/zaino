//! [`MockChain`]: the one regtest block tree every test takes its blocks, headers and fees from
//!
//! Every block handed out passes M1–M7 (else panic naming the rule; `usage.md` § Invariants):
//! - M1: hash = SHA-256d(`encode_header`)
//! - M2: linked to parent, height + 1, time > parent's median time past (`time()`)
//! - M3: merkle root = `MerkleRoot::of_txids`, txids distinct per branch (`Ledger::apply`)
//! - M4: one coinbase, slot 0, spends nothing, states no fee (`plan()`, `Ledger::apply`)
//! - M5: nBits = regtest limit, or `outweigh()` = least work that becomes best (`bits()`)
//! - M6: spends unspent on own branch, nullifiers fresh, pools >= 0 (ZIP 209), fee = derived
//! - M7: pool data only once its upgrade active (Sapling, NU5 = Orchard, NU6.3 = Ironwood)
//! - Mempool: each entry admitted (M6, M7) on the best after the ones before it (`repool()`)

use std::collections::HashMap;
use std::sync::Arc;

use zcash_protocol::consensus::{
    target_spacing, BlockHeight, BranchId, NetworkType, NetworkUpgrade, Parameters, H0,
};
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::TxId;

use super::build::{BlockBuilder, Planned, TxBuilder};
use super::ledger::{Ledger, Unmined};
use super::upgrades::{all_at, info};
use super::{encode_header, h, header_hash, sha256d, GENESIS_TIME, REGTEST_BITS};
use crate::protocol::median_time_past;
use crate::types::{
    Block, BlockFees, BlockHash, BlockRef, BlockchainInfo, CompactDifficulty, ConsensusBranchIds,
    DecodedHeader, EquihashSolution, Fee, MerkleRoot, Transaction,
};

/// nBits source: `Limit` = the regtest limit everywhere (strict header rules, heavier = longer);
/// `Varied` = `Branch::outweigh` / `BlockBuilder::bits` allowed (header views accept any nBits)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    Limit,
    Varied,
}

/// What the views derive their parameters from (`network` = a label: schemas, addresses, params)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub upgrades: LocalNetwork,
    pub work: Work,
    pub network: NetworkType,
}

/// Block tree over one genesis; best chain = the most-work tip, first mined on a tie
#[derive(Debug)]
pub struct MockChain {
    schedule: Schedule,
    genesis_spec: BlockBuilder,
    blocks: HashMap<BlockHash, Mined>,
    genesis: BlockHash,
    best: BlockHash,
    bytes: HashMap<TxId, Vec<u8>>,
    mempool: Vec<Unmined>,
    nonces: u64,
    txids: u64,
}

#[derive(Debug)]
struct Mined {
    block: Arc<Block>,
    fees: BlockFees,
    cumulative_work: u128,
}

impl MockChain {
    /// Every upgrade through NU6.3 at height 1, `Work::Limit`, a bare-coinbase genesis
    pub fn regtest() -> Self {
        let upgrades = all_at(h(1));
        let schedule = Schedule { upgrades, work: Work::Limit, network: NetworkType::Regtest };
        Self::over(schedule, BlockBuilder::new())
    }

    /// Before the first mine; heights in protocol order (zebrad refuses any other config)
    pub fn upgrades(self, upgrades: LocalNetwork) -> Self {
        let upgrades = LocalNetwork::from_activation_heights(|nu| upgrades.activation_height(nu));
        let schedule = Schedule { upgrades, ..self.schedule };
        self.reconfigured(schedule, None)
    }

    /// `Work::Varied`, before the first mine (every reorg-by-work test declares it)
    pub fn varied_work(self) -> Self {
        let schedule = Schedule { work: Work::Varied, ..self.schedule };
        self.reconfigured(schedule, None)
    }

    /// Before the first mine
    pub fn network(self, network: NetworkType) -> Self {
        let schedule = Schedule { network, ..self.schedule };
        self.reconfigured(schedule, None)
    }

    /// Before the first mine
    pub fn genesis_with(self, block: impl FnOnce(BlockBuilder) -> BlockBuilder) -> Self {
        let schedule = self.schedule;
        self.reconfigured(schedule, Some(block(BlockBuilder::new())))
    }

    pub fn genesis(&self) -> BlockRef {
        self.reference(self.genesis)
    }

    pub fn tip(&self) -> BlockRef {
        self.reference(self.best)
    }

    /// Best chain at `height`
    pub fn at(&self, height: BlockHeight) -> BlockRef {
        let tip = self.tip();
        assert!(height <= tip.height, "{height} above the best tip {}", tip.height);
        self.ancestor(tip, height)
    }

    /// `tip`'s path at `height` (≤ `tip.height`)
    pub fn ancestor(&self, tip: BlockRef, height: BlockHeight) -> BlockRef {
        assert!(height <= tip.height, "{height} above the tip {}", tip.height);
        let mut at = tip;
        while at.height > height {
            at = self.reference(self.block(at.hash).header().prev_hash);
        }
        at
    }

    /// Highest block on both `a`'s and `b`'s paths (`a` itself when `b` extends it)
    pub fn fork_point(&self, a: BlockRef, b: BlockRef) -> BlockRef {
        let low = a.height.min(b.height);
        let (mut a, mut b) = (self.ancestor(a, low), self.ancestor(b, low));
        while a != b {
            a = self.reference(self.block(a.hash).header().prev_hash);
            b = self.reference(self.block(b.hash).header().prev_hash);
        }
        a
    }

    /// On `tip()`
    pub fn mine(&mut self, block: impl FnOnce(BlockBuilder) -> BlockBuilder) -> BlockRef {
        let tip = self.tip();
        self.branch(tip).mine(block).tip()
    }

    /// `count` bare blocks on `tip()`; the last (`tip()` itself when `count` = 0)
    pub fn mine_empty(&mut self, count: u32) -> BlockRef {
        let tip = self.tip();
        self.branch(tip).mine_empty(count).tip()
    }

    /// Mining on any held block
    pub fn branch(&mut self, parent: BlockRef) -> Branch<'_> {
        assert_eq!(self.reference(parent.hash), parent, "branch parent held at its height");
        Branch { chain: self, tip: parent, outweigh: false }
    }

    /// `branch(at(at))`
    pub fn fork(&mut self, at: BlockHeight) -> Branch<'_> {
        let parent = self.at(at);
        self.branch(parent)
    }

    pub fn block(&self, hash: BlockHash) -> &Arc<Block> {
        &self.mined(hash).block
    }

    /// Genesis ..= `tip`
    pub fn blocks(&self, tip: BlockRef) -> Vec<Arc<Block>> {
        assert_eq!(self.reference(tip.hash), tip, "tip held at its height");
        let mut path = vec![Arc::clone(self.block(tip.hash))];
        while let Some(parent) = path.last().and_then(|b| self.blocks.get(&b.header().prev_hash)) {
            path.push(Arc::clone(&parent.block));
        }
        path.reverse();
        path
    }

    /// Consensus encoding
    pub fn header_bytes(&self, hash: BlockHash) -> Vec<u8> {
        encode_header(self.block(hash).header())
    }

    pub fn fees(&self, hash: BlockHash) -> BlockFees {
        self.mined(hash).fees.clone()
    }

    /// Bytes of a `raw_tx` (`None` = built by `TxBuilder`: no bytes exist)
    pub fn tx_bytes(&self, txid: TxId) -> Option<&[u8]> {
        self.bytes.get(&txid).map(Vec::as_slice)
    }

    /// `getblockchaininfo` with `tip` as the validator's best
    pub fn blockchain_info(&self, tip: BlockRef) -> BlockchainInfo {
        assert_eq!(self.reference(tip.hash), tip, "tip held at its height");
        let upgrades = &self.schedule.upgrades;
        let sapling = upgrades.activation_height(NetworkUpgrade::Sapling);
        BlockchainInfo {
            blocks: tip.height,
            estimated_height: tip.height,
            best_block_hash: tip.hash,
            sapling_activation: sapling.expect("a Sapling activation (zebrad always has one)"),
            upgrades: info(upgrades, tip.height),
            consensus: ConsensusBranchIds {
                chain_tip: BranchId::for_height(upgrades, tip.height),
                next_block: BranchId::for_height(upgrades, tip.height + 1),
            },
        }
    }

    pub fn schedule(&self) -> Schedule {
        self.schedule
    }

    /// Unspent outputs, txids, nullifiers and pool values on genesis ..= `tip`
    pub fn ledger(&self, tip: BlockRef) -> Ledger {
        let mut ledger = Ledger::empty(self.schedule.upgrades, H0);
        for block in self.blocks(tip) {
            for tx in block.transactions() {
                ledger.apply(tx, None).expect("a held block passed M3–M7 when mined");
            }
            ledger.advance();
        }
        ledger
    }

    /// Held for the next block on the best: admitted after every entry before it (M6, M7), else
    /// panic naming the refusal
    pub fn submit(&mut self, (tx, bytes): (Transaction, Vec<u8>)) -> TxId {
        let mut pool = self.pool_ledger();
        let fee = pool.admit(&tx).unwrap_or_else(|refused| panic!("submit {}: {refused}", tx.txid));
        let txid = tx.txid;
        self.mempool.push(Unmined { tx, bytes, fee });
        txid
    }

    /// Submission order
    pub fn mempool(&self) -> &[Unmined] {
        &self.mempool
    }

    /// Out of the mempool, with every entry spending its outputs
    pub fn evict(&mut self, txid: TxId) {
        self.mempool.retain(|unmined| unmined.tx.txid != txid);
        let best = self.best;
        self.repool(best);
    }

    /// One block on `tip()` carrying every mempool entry, in submission order
    pub fn mine_mempool(&mut self) -> BlockRef {
        let held = self.mempool.clone();
        let carried = |block: BlockBuilder| {
            held.into_iter().fold(block, |block, unmined| block.raw_tx((unmined.tx, unmined.bytes)))
        };
        self.mine(carried)
    }

    fn over(schedule: Schedule, genesis_spec: BlockBuilder) -> Self {
        let mut chain = Self {
            schedule,
            genesis_spec: genesis_spec.clone(),
            blocks: HashMap::new(),
            genesis: BlockHash::ZERO,
            best: BlockHash::ZERO,
            bytes: HashMap::new(),
            mempool: Vec::new(),
            nonces: 0,
            txids: 0,
        };
        let genesis = chain.insert(None, genesis_spec, false);
        (chain.genesis, chain.best) = (genesis.hash, genesis.hash);
        chain
    }

    /// Genesis rebuilt under the new configuration (nothing mined on it yet)
    fn reconfigured(self, schedule: Schedule, genesis: Option<BlockBuilder>) -> Self {
        assert_eq!(self.blocks.len(), 1, "MockChain configured after its first mine");
        Self::over(schedule, genesis.unwrap_or(self.genesis_spec))
    }

    fn mined(&self, hash: BlockHash) -> &Mined {
        self.blocks.get(&hash).unwrap_or_else(|| panic!("block {hash} not mined by this MockChain"))
    }

    fn reference(&self, hash: BlockHash) -> BlockRef {
        BlockRef { hash, height: self.block(hash).header().height }
    }

    /// Child of `parent` (`None` = genesis) holding `spec`, checked (M1–M7), then held
    fn insert(&mut self, parent: Option<BlockRef>, spec: BlockBuilder, outweigh: bool) -> BlockRef {
        let height = parent.map_or(H0, |parent| parent.height + 1);
        let time = self.time(parent, height, spec.time);
        let bits = self.bits(parent, spec.bits, outweigh);
        let mut ledger = match (parent, spec.reads_history()) {
            (Some(parent), true) => self.ledger(parent),
            _ => Ledger::empty(self.schedule.upgrades, height),
        };
        let planned = self.plan(spec);
        let fees = planned.iter().map(|(tx, stated)| {
            let fee = ledger.apply(tx, *stated);
            fee.unwrap_or_else(|refused| panic!("{} at {height}: {refused}", tx.txid))
        });
        let fees: Vec<Fee> = fees.collect();
        let transactions: Vec<Transaction> = planned.into_iter().map(|(tx, _)| tx).collect();
        let txids: Vec<TxId> = transactions.iter().map(|tx| tx.txid).collect();

        let mut nonce = [0u8; 32];
        nonce[..8].copy_from_slice(&self.nonces.to_le_bytes());
        self.nonces += 1;
        let mut header = DecodedHeader {
            hash: BlockHash::ZERO,
            version: 4,
            prev_hash: parent.map_or(BlockHash::ZERO, |parent| parent.hash),
            height,
            time,
            merkle_root: MerkleRoot::of_txids(&txids).expect("txids distinct (M3)"),
            block_commitments: [0u8; 32].into(),
            bits,
            nonce,
            solution: EquihashSolution::Regtest([0u8; 36]),
        };
        header.hash = header_hash(&header);
        let parent_work = parent.map_or(0, |parent| self.mined(parent.hash).cumulative_work);
        let cumulative_work =
            parent_work.checked_add(bits.work()).expect("cumulative work past u128");

        let block = BlockRef { hash: header.hash, height };
        let fees = BlockFees { height, hash: block.hash, fees };
        let mined =
            Mined { block: Arc::new(Block::new(header, transactions)), fees, cumulative_work };
        assert!(self.blocks.insert(block.hash, mined).is_none(), "{} mined twice", block.hash);
        if parent.is_some() && cumulative_work > self.mined(self.best).cumulative_work {
            let old = std::mem::replace(&mut self.best, block.hash);
            self.repool(old);
        }
        block
    }

    /// Best ledger + every mempool entry
    fn pool_ledger(&self) -> Ledger {
        let mut pool = self.ledger(self.tip());
        for unmined in &self.mempool {
            pool.admit(&unmined.tx).expect("mempool entries admitted in order (repool)");
        }
        pool
    }

    /// Mempool re-admitted on the best after it left `old`
    ///
    /// - `old`'s transactions above the fork come back first, block order (coinbases and
    ///   `TxBuilder` ones never: no bytes); mined, conflicting or orphaned entries leave
    fn repool(&mut self, old: BlockHash) {
        let old = self.reference(old);
        let fork = self.fork_point(old, self.tip());
        let mut above_fork = Vec::new();
        let mut at = old;
        while at != fork {
            let block = self.block(at.hash);
            above_fork.push(Arc::clone(block));
            at = self.reference(block.header().prev_hash);
        }
        let unmined = above_fork.iter().rev().flat_map(|block| block.transactions().iter().skip(1));
        let returned =
            unmined.filter_map(|tx| Some((tx.clone(), self.bytes.get(&tx.txid)?.clone())));
        let returned: Vec<(Transaction, Vec<u8>)> = returned.collect();
        if returned.is_empty() && self.mempool.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.mempool).into_iter();
        let candidates =
            returned.into_iter().chain(held.map(|unmined| (unmined.tx, unmined.bytes)));
        self.mempool = self.ledger(self.tip()).readmit(candidates);
    }

    /// Coinbase first, each beside its stated fee; default txids minted
    ///
    /// - `raw_tx` bytes held at once (keyed by the txid they hash to: never wrong for any block)
    fn plan(&mut self, spec: BlockBuilder) -> Vec<(Transaction, Option<u64>)> {
        let coinbase_txid = self.txid(&spec.coinbase);
        let coinbase_fee = spec.coinbase.fee;
        let mut planned = vec![(spec.coinbase.into_transaction(coinbase_txid, true), coinbase_fee)];
        for tx in spec.txs {
            planned.push(match tx {
                Planned::Built(tx) => {
                    let (txid, stated) = (self.txid(&tx), tx.fee);
                    (tx.into_transaction(txid, false), stated)
                }
                Planned::Raw(tx, raw) => {
                    assert!(!tx.transparent.coinbase, "{}: raw_tx is never the coinbase", tx.txid);
                    self.bytes.insert(tx.txid, raw);
                    (tx, None)
                }
            });
        }
        planned
    }

    /// Stated, else SHA-256d(mint counter ‖ content)
    fn txid(&mut self, tx: &TxBuilder) -> TxId {
        if let Some(txid) = tx.txid {
            return txid;
        }
        let minted = [&self.txids.to_le_bytes()[..], &tx.content()].concat();
        self.txids += 1;
        TxId::from_bytes(sha256d(&minted))
    }

    /// M2: stated or parent + target spacing, after the parent's median time past
    fn time(&self, parent: Option<BlockRef>, height: BlockHeight, stated: Option<u32>) -> u32 {
        let Some(parent) = parent else { return stated.unwrap_or(GENESIS_TIME) };
        let parent_time = self.block(parent.hash).header().time;
        let time = stated.unwrap_or(parent_time + target_spacing(&self.schedule.upgrades, height));
        let ancestors = std::iter::successors(Some(self.block(parent.hash)), |block| {
            self.blocks.get(&block.header().prev_hash).map(|mined| &mined.block)
        });
        let times = ancestors.map(|block| block.header().time);
        let median = median_time_past(times).expect("a parent holds a time");
        assert!(time > median, "time {time} at {height}: not after the median time past {median}");
        time
    }

    /// M5: the limit, a stated nBits (`Work::Varied`), or just enough work to become best
    fn bits(
        &self,
        parent: Option<BlockRef>,
        stated: Option<u32>,
        outweigh: bool,
    ) -> CompactDifficulty {
        let valid = |bits: u32| {
            CompactDifficulty::try_from_bits(bits).unwrap_or_else(|e| panic!("bits(): {e}"))
        };
        match (stated, outweigh, parent) {
            (Some(_), true, _) => {
                panic!("bits() on an outweigh() block (outweigh picks its nBits)")
            }
            (Some(_), false, _) if self.schedule.work == Work::Limit => {
                panic!("bits() under Work::Limit (declare varied_work())")
            }
            (Some(bits), false, _) => valid(bits),
            (None, true, Some(parent)) => {
                let best = self.mined(self.best).cumulative_work;
                let parent = self.mined(parent.hash).cumulative_work;
                lightest_over(best.saturating_sub(parent))
            }
            (None, _, _) => valid(REGTEST_BITS),
        }
    }
}

/// Mining on one block of a [`MockChain`], each `mine` moving it to the new block
#[derive(Debug)]
pub struct Branch<'c> {
    chain: &'c mut MockChain,
    tip: BlockRef,
    outweigh: bool,
}

impl Branch<'_> {
    /// Next block: the least work that makes it the best (`Work::Varied`)
    pub fn outweigh(self) -> Self {
        let work = self.chain.schedule.work;
        assert_eq!(work, Work::Varied, "outweigh() under Work::Limit (declare varied_work())");
        Self { outweigh: true, ..self }
    }

    pub fn mine(self, block: impl FnOnce(BlockBuilder) -> BlockBuilder) -> Self {
        let tip = self.chain.insert(Some(self.tip), block(BlockBuilder::new()), self.outweigh);
        Self { chain: self.chain, tip, outweigh: false }
    }

    pub fn mine_empty(self, count: u32) -> Self {
        (0..count).fold(self, |branch, _| branch.mine(|block| block))
    }

    pub fn tip(&self) -> BlockRef {
        self.tip
    }
}

/// Easiest nBits whose work exceeds `over`, never easier than the limit
///
/// - canonical encodings (mantissa `0x8000..=0x7fffff`, exponent `0x12..=0x20`) rise in target
///   with `(exponent, mantissa)`: binary search for the last whose work still exceeds `over`
fn lightest_over(over: u128) -> CompactDifficulty {
    const MANTISSAS: u64 = 0x7f_ffff - 0x8000 + 1;
    let bits = |index: u64| {
        let (exponent, mantissa) = (0x12 + index / MANTISSAS, 0x8000 + index % MANTISSAS);
        let bits = u32::try_from(exponent << 24 | mantissa).expect("exponent ≤ 0x20");
        CompactDifficulty::try_from_bits(bits).expect("canonical encoding in range")
    };
    let limit = (0x20 - 0x12) * MANTISSAS + (u64::from(REGTEST_BITS & 0xff_ffff) - 0x8000);
    if bits(limit).work() > over {
        return bits(limit);
    }
    assert!(bits(0).work() > over, "work {over} past the heaviest nBits");
    let (mut beats, mut loses) = (0, limit);
    while loses - beats > 1 {
        let mid = beats + (loses - beats) / 2;
        match bits(mid).work() > over {
            true => beats = mid,
            false => loses = mid,
        }
    }
    bits(beats)
}

#[cfg(test)]
mod tests;
