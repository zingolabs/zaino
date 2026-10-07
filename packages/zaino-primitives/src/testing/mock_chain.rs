//! [`MockChain`]: the one regtest block tree every test takes its blocks, headers and fees from
//!
//! - invariants M1–M7 (`docs/design/mock-chain.md` §4): every block handed out passes them

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use zcash_protocol::consensus::{NetworkType, NetworkUpgrade};

use super::build::{BlockBuilder, Planned, TxBuilder};
use super::upgrades::Upgrades;
use super::{
    balances, encode_header, fee_left, h, header_hash, sha256d, GENESIS_TIME, REGTEST_BITS,
};
use crate::types::{
    Block, BlockFees, BlockHash, BlockHeader, BlockRef, BlockchainInfo, CompactDifficulty,
    ConsensusBranchIds, EquihashSolution, Fee, Height, MerkleRoot, Nullifier, OutPoint,
    ShieldedPool, Transaction, TransactionId, Zatoshis,
};

/// zebra-state `check.rs`: a header's time must pass the median of this many ancestors' times
const MEDIAN_SPAN: usize = 11;

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
    pub upgrades: Upgrades,
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
    bytes: HashMap<TransactionId, Vec<u8>>,
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
        let upgrades = Upgrades::all_at(h(1));
        let schedule = Schedule { upgrades, work: Work::Limit, network: NetworkType::Regtest };
        Self::over(schedule, BlockBuilder::new())
    }

    /// Before the first mine
    pub fn upgrades(self, upgrades: Upgrades) -> Self {
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
    pub fn at(&self, height: Height) -> BlockRef {
        let tip = self.tip();
        assert!(height <= tip.height, "{height} above the best tip {}", tip.height);
        let mut at = tip;
        while at.height > height {
            at = self.reference(self.block(at.hash).header().prev_hash);
        }
        at
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
    pub fn fork(&mut self, at: Height) -> Branch<'_> {
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
    pub fn tx_bytes(&self, txid: TransactionId) -> Option<&[u8]> {
        self.bytes.get(&txid).map(Vec::as_slice)
    }

    /// `getblockchaininfo` with `tip` as the validator's best
    pub fn blockchain_info(&self, tip: BlockRef) -> BlockchainInfo {
        assert_eq!(self.reference(tip.hash), tip, "tip held at its height");
        let upgrades = &self.schedule.upgrades;
        let sapling = upgrades.activation(NetworkUpgrade::Sapling);
        BlockchainInfo {
            blocks: tip.height,
            estimated_height: tip.height,
            best_block_hash: tip.hash,
            sapling_activation: sapling.expect("a Sapling activation (zebrad always has one)"),
            upgrades: upgrades.info(tip.height),
            consensus: ConsensusBranchIds {
                chain_tip: upgrades.branch_at(tip.height),
                next_block: upgrades.branch_at(tip.height.next()),
            },
        }
    }

    pub fn schedule(&self) -> Schedule {
        self.schedule
    }

    fn over(schedule: Schedule, genesis_spec: BlockBuilder) -> Self {
        schedule.upgrades.assert_ordered();
        let mut chain = Self {
            schedule,
            genesis_spec: genesis_spec.clone(),
            blocks: HashMap::new(),
            genesis: BlockHash::ZERO,
            best: BlockHash::ZERO,
            bytes: HashMap::new(),
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
        let height = parent.map_or(Height::GENESIS, |parent| parent.height.next());
        let time = self.time(parent, height, spec.time);
        let bits = self.bits(parent, spec.bits, outweigh);
        let mut ledger = match (parent, spec.reads_history()) {
            (Some(parent), true) => self.ledger(parent),
            _ => Ledger::default(),
        };
        let planned = self.plan(spec);
        let upgrades = self.schedule.upgrades;
        let fees = planned.iter().map(|(tx, stated)| ledger.admit(tx, *stated, height, &upgrades));
        let fees: Vec<Fee> = fees.collect();
        let transactions: Vec<Transaction> = planned.into_iter().map(|(tx, _)| tx).collect();
        let txids: Vec<TransactionId> = transactions.iter().map(|tx| tx.txid).collect();

        let mut nonce = [0u8; 32];
        nonce[..8].copy_from_slice(&self.nonces.to_le_bytes());
        self.nonces += 1;
        let mut header = BlockHeader {
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
            self.best = block.hash;
        }
        block
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
    fn txid(&mut self, tx: &TxBuilder) -> TransactionId {
        if let Some(txid) = tx.txid {
            return txid;
        }
        let minted = [&self.txids.to_le_bytes()[..], &tx.content()].concat();
        self.txids += 1;
        TransactionId::from(sha256d(&minted))
    }

    /// M2: stated or parent + target spacing, after the parent's median time past
    fn time(&self, parent: Option<BlockRef>, height: Height, stated: Option<u32>) -> u32 {
        let Some(parent) = parent else { return stated.unwrap_or(GENESIS_TIME) };
        let parent_time = self.block(parent.hash).header().time;
        let time = stated.unwrap_or(parent_time + self.schedule.upgrades.spacing(height));
        let mut times: Vec<u32> = Vec::with_capacity(MEDIAN_SPAN);
        let mut at = Some(parent.hash);
        while let Some(block) = at.and_then(|hash| self.blocks.get(&hash)).map(|m| &m.block) {
            times.push(block.header().time);
            at = (times.len() < MEDIAN_SPAN).then_some(block.header().prev_hash);
        }
        times.sort_unstable();
        let median = times[times.len() / 2];
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

    /// Unspent outputs, txids, nullifiers and pool values on genesis ..= `tip`
    fn ledger(&self, tip: BlockRef) -> Ledger {
        let mut ledger = Ledger::default();
        for block in self.blocks(tip) {
            let height = block.header().height;
            for tx in block.transactions() {
                ledger.admit(tx, None, height, &self.schedule.upgrades);
            }
        }
        ledger
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

/// One branch's state at a block: what the next transaction may spend and reveal
#[derive(Default)]
struct Ledger {
    unspent: HashMap<OutPoint, Zatoshis>,
    txids: HashSet<TransactionId>,
    nullifiers: HashSet<(ShieldedPool, Nullifier)>,
    /// ZIP 209 chain value pools: sprout, sapling, orchard, ironwood
    pools: [i64; 4],
}

impl Ledger {
    /// M3 (distinct txids), M4 (coinbase spends nothing), M6 (spends, conservation, fee), M7 (pools)
    fn admit(
        &mut self,
        tx: &Transaction,
        stated: Option<u64>,
        at: Height,
        upgrades: &Upgrades,
    ) -> Fee {
        let txid = tx.txid;
        assert!(self.txids.insert(txid), "{txid} at {at}: txid repeated on this branch");
        let balances = balances(tx);
        let pools = [
            (NetworkUpgrade::Sapling, tx.sapling.spends.len() + tx.sapling.outputs.len()),
            (NetworkUpgrade::Nu5, tx.orchard.actions.len()),
            (NetworkUpgrade::Nu6_3, tx.ironwood.actions.len()),
        ];
        for ((upgrade, data), balance) in pools.into_iter().zip(&balances[1..]) {
            let used = data > 0 || *balance != 0;
            let active = upgrades.active(upgrade, at);
            assert!(!used || active, "{txid} at {at}: pool data before {upgrade}");
        }
        for (pool, nullifier) in nullifiers(tx) {
            let fresh = self.nullifiers.insert((pool, nullifier));
            assert!(fresh, "{txid} at {at}: {pool} nullifier revealed twice on this branch");
        }

        let coinbase = tx.transparent.coinbase;
        let withdraws = balances.iter().any(|balance| *balance > 0);
        let spends = !tx.transparent.inputs.is_empty() || !tx.sapling.spends.is_empty();
        assert!(!coinbase || !(spends || withdraws), "{txid} at {at}: a coinbase spends nothing");
        assert!(!coinbase || stated.is_none(), "{txid} at {at}: a coinbase pays no fee");

        let mut spent: i64 = 0;
        for prevout in &tx.transparent.inputs {
            let (from, vout) = (prevout.txid, prevout.vout);
            let value = self.unspent.remove(prevout).unwrap_or_else(|| {
                panic!("{txid} at {at}: spends {from}:{vout}, not unspent on this branch")
            });
            spent += value.as_i64();
        }
        for (pool, balance) in self.pools.iter_mut().zip(balances) {
            *pool -= balance;
            let held = *pool >= 0;
            assert!(held, "{txid} at {at}: takes {balance} from a pool holding less (ZIP 209)");
        }
        for (vout, output) in (0..).zip(&tx.transparent.outputs) {
            self.unspent.insert(OutPoint { txid, vout }, output.value);
        }
        if coinbase {
            return Fee::Coinbase;
        }
        let remaining = fee_left(tx, spent);
        let fee = u64::try_from(remaining)
            .unwrap_or_else(|_| panic!("{txid} at {at}: overspends by {}", -remaining));
        if let Some(stated) = stated {
            assert_eq!(stated, fee, "{txid} at {at}: stated fee != inputs − outputs + balances");
        }
        Fee::Paid(Zatoshis::new(fee).expect("a fee within the supply"))
    }
}

fn nullifiers(tx: &Transaction) -> impl Iterator<Item = (ShieldedPool, Nullifier)> + '_ {
    let sapling = tx.sapling.spends.iter().map(|spend| (ShieldedPool::Sapling, spend.nullifier));
    let orchard = tx.orchard.actions.iter().map(|action| (ShieldedPool::Orchard, action.nullifier));
    let ironwood =
        tx.ironwood.actions.iter().map(|action| (ShieldedPool::Ironwood, action.nullifier));
    sapling.chain(orchard).chain(ironwood)
}

#[cfg(test)]
mod tests;
