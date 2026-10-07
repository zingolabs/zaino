//! Regtest chains with real header bytes: hash = SHA-256d of the header, `prev_hash` links, merkle
//! root = the txids' (`cfg(test)` too: a crate's own features don't self-enable)

use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::types::{
    Block, BlockHash, BlockHeader, BlockRef, CompactDifficulty, EquihashSolution, Height,
    MerkleRoot, Transaction, TransactionId, TransparentData,
};

/// zcashd regtest `powLimit` as nBits (the only nBits regtest accepts)
const REGTEST_BITS: u32 = 0x200f_0f0f;
const GENESIS_TIME: u32 = 1_700_000_000;
/// Post-Blossom target spacing, seconds
const SPACING: u32 = 75;

/// Consensus bytes of `header` (`hash` + `height` derived from them, never encoded)
pub fn encode_header(header: &BlockHeader) -> Vec<u8> {
    let solution = header.solution.as_bytes();
    let mut raw = Vec::with_capacity(143 + solution.len());
    raw.extend(header.version.to_le_bytes());
    raw.extend(<[u8; 32]>::from(header.prev_hash));
    raw.extend(<[u8; 32]>::from(header.merkle_root));
    raw.extend(<[u8; 32]>::from(header.block_commitments));
    raw.extend(header.time.to_le_bytes());
    raw.extend(header.bits.bits().to_le_bytes());
    raw.extend(header.nonce);
    match header.solution {
        EquihashSolution::Standard(_) => raw.extend([0xfd, 0x40, 0x05]),
        EquihashSolution::Regtest(_) => raw.push(36),
    }
    raw.extend(solution);
    raw
}

/// What `header.hash` must be: SHA-256d of [`encode_header`]
pub fn header_hash(header: &BlockHeader) -> BlockHash {
    BlockHash::from(sha256d(&encode_header(header)))
}

fn sha256d(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(bytes)).into()
}

/// Bare coinbase, txid = SHA-256d of the mint counter
fn coinbase(minted: u64) -> Transaction {
    Transaction {
        txid: TransactionId::from(sha256d(&minted.to_le_bytes())),
        transparent: TransparentData { coinbase: true, ..TransparentData::default() },
        sprout: Default::default(),
        sapling: Default::default(),
        orchard: Default::default(),
        ironwood: Default::default(),
    }
}

/// One branch from its own genesis, block `i` holding exactly `blocks[i]` (index sinks take
/// `Arc<Block>`)
pub fn linked(blocks: impl IntoIterator<Item = Vec<Transaction>>) -> Vec<Arc<Block>> {
    let mut blocks = blocks.into_iter();
    let mut chain = Chain::with_genesis(blocks.next().expect("a genesis"));
    let tip = blocks.fold(chain.genesis(), |tip, txs| chain.mine_with(tip.hash, txs));
    chain.path(tip.hash).into_iter().map(Arc::new).collect()
}

/// Block tree on one regtest genesis: mine on any held block (a branch at any height)
///
/// - version 4, regtest nBits (any via `mine_bits`), zero 36-byte solution, time = parent + 75 s
/// - nonce + default coinbase txid from a mint counter (siblings never collide)
#[derive(Debug, Clone)]
pub struct Chain {
    blocks: HashMap<BlockHash, Block>,
    genesis: BlockHash,
    minted: u64,
}

impl Default for Chain {
    fn default() -> Self {
        Self::new()
    }
}

impl Chain {
    pub fn new() -> Self {
        Self::with_genesis(vec![coinbase(0)])
    }

    /// Genesis holding exactly `transactions`
    pub fn with_genesis(transactions: Vec<Transaction>) -> Self {
        let mut chain = Self { blocks: HashMap::new(), genesis: BlockHash::ZERO, minted: 0 };
        chain.genesis = chain.insert(None, GENESIS_TIME, REGTEST_BITS, transactions).hash;
        chain
    }

    pub fn genesis(&self) -> BlockRef {
        BlockRef { hash: self.genesis, height: Height::GENESIS }
    }

    /// Bare coinbase child of `parent`
    pub fn mine(&mut self, parent: BlockHash) -> BlockRef {
        let time = self.block(parent).header().time + SPACING;
        self.mine_at(parent, time)
    }

    /// Bare coinbase child of `parent` at `time` (rule tests: early, late)
    pub fn mine_at(&mut self, parent: BlockHash, time: u32) -> BlockRef {
        self.mine_bits(parent, time, REGTEST_BITS)
    }

    /// Bare coinbase child of `parent` at `time` under `bits` (work that varies per branch)
    pub fn mine_bits(&mut self, parent: BlockHash, time: u32, bits: u32) -> BlockRef {
        self.insert(Some(parent), time, bits, vec![coinbase(self.minted)])
    }

    /// Bare child of `parent` outweighing all of `over` (< 256 blocks) together: nBits exponent one
    /// below the heaviest's (256× its work); `None` = past what a `u128` cumulative work holds
    pub fn mine_heavier(&mut self, parent: BlockHash, over: &[BlockHash]) -> Option<BlockRef> {
        let exponent = |hash: &BlockHash| self.block(*hash).header().bits.bits() >> 24;
        let heaviest = over.iter().map(exponent).min().unwrap_or(REGTEST_BITS >> 24);
        let exponent = heaviest.checked_sub(1).filter(|exponent| *exponent >= 0x12)?;
        let time = self.block(parent).header().time + SPACING;
        Some(self.mine_bits(parent, time, (exponent << 24) | (REGTEST_BITS & 0x00ff_ffff)))
    }

    /// Child of `parent` holding exactly `transactions` (slot 0 = the coinbase slot)
    pub fn mine_with(&mut self, parent: BlockHash, transactions: Vec<Transaction>) -> BlockRef {
        let time = self.block(parent).header().time + SPACING;
        self.insert(Some(parent), time, REGTEST_BITS, transactions)
    }

    /// `count` bare blocks on `parent`; the last (`parent` itself when `count` = 0)
    pub fn extend(&mut self, parent: BlockHash, count: u32) -> BlockRef {
        let start = BlockRef { hash: parent, height: self.block(parent).header().height };
        (0..count).fold(start, |tip, _| self.mine(tip.hash))
    }

    pub fn block(&self, hash: BlockHash) -> &Block {
        self.blocks.get(&hash).expect("block mined by this chain")
    }

    /// Genesis ..= `tip`, in height order
    pub fn path(&self, tip: BlockHash) -> Vec<Block> {
        let mut path = vec![self.block(tip).clone()];
        while let Some(parent) = path.last().and_then(|b| self.blocks.get(&b.header().prev_hash)) {
            path.push(parent.clone());
        }
        path.reverse();
        path
    }

    fn insert(
        &mut self,
        parent: Option<BlockHash>,
        time: u32,
        bits: u32,
        transactions: Vec<Transaction>,
    ) -> BlockRef {
        let (prev_hash, height) = match parent {
            Some(parent) => (parent, self.block(parent).header().height.next()),
            None => (BlockHash::ZERO, Height::GENESIS),
        };
        let mut nonce = [0u8; 32];
        nonce[..8].copy_from_slice(&self.minted.to_le_bytes());
        self.minted += 1;
        let txids: Vec<TransactionId> = transactions.iter().map(|tx| tx.txid).collect();
        let mut header = BlockHeader {
            hash: BlockHash::ZERO,
            version: 4,
            prev_hash,
            height,
            time,
            merkle_root: MerkleRoot::of_txids(&txids).expect("a coinbase, txids distinct"),
            block_commitments: [0u8; 32].into(),
            bits: CompactDifficulty::try_from_bits(bits).expect("a valid compact target"),
            nonce,
            solution: EquihashSolution::Regtest([0u8; 36]),
        };
        header.hash = header_hash(&header);
        let block = BlockRef { hash: header.hash, height };
        self.blocks.insert(block.hash, Block::new(header, transactions));
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Genesis pinned byte for byte (177 = regtest header); each child links to its parent, one
    /// height and 75 s up; siblings on one parent differ; `path` = genesis ..= tip
    #[test]
    fn headers_are_real_bytes_linked_by_hash_and_siblings_never_collide() {
        let mut chain = Chain::new();
        let genesis = chain.block(chain.genesis().hash).header().clone();
        let raw = encode_header(&genesis);
        assert_eq!(raw.len(), 177);
        let golden = [
            "04000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "7ef0ca626bbb058dd443bb78e33b888bdec8295c96e51f5545f96370870c10b9",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "00f15365",
            "0f0f0f20",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "24000000000000000000000000000000000000000000000000000000000000000000000000",
        ];
        assert_eq!(hex(&raw), golden.concat());
        let coinbase = <[u8; 32]>::from(chain.block(genesis.hash).transactions()[0].txid);
        assert_eq!(coinbase, sha256d(&0u64.to_le_bytes()), "merkle root of one = its txid");
        let display = "2c7c50c5b6ed3a223ec575e024891141c64d2a6469a8db045d53229e06aa7e7e";
        assert_eq!(genesis.hash.to_string(), display);
        assert_eq!(BlockHash::from(sha256d(&raw)), genesis.hash);

        let one = chain.mine(genesis.hash);
        let sibling = chain.mine(genesis.hash);
        let two = chain.extend(one.hash, 1);
        assert_ne!(one.hash, sibling.hash, "same parent, same time: distinct blocks");
        let child = chain.block(two.hash).header();
        assert_eq!(
            (child.prev_hash, u32::from(child.height), child.time),
            (one.hash, 2, GENESIS_TIME + 2 * SPACING)
        );
        let coinbase = chain.block(two.hash).transactions()[0].txid;
        assert_eq!(child.merkle_root, MerkleRoot::from(<[u8; 32]>::from(coinbase)));
        let path: Vec<BlockHash> = chain.path(two.hash).iter().map(|b| b.header().hash).collect();
        assert_eq!(path, [genesis.hash, one.hash, two.hash]);
        assert_eq!(chain.extend(two.hash, 0), two);
    }
}
