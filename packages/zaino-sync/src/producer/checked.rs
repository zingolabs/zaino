//! A fetched block against the verified header naming it (`verified-chain.md` §5)
//!
//! - Hash = the header's (recomputed from its bytes at decode), height = its coinbase's, merkle
//!   root rebuilt from its txids = the header's
//! - Mismatch = that source misanswered, never an invalid header (ZIP 256: a v5 hash does not
//!   commit to authorizing data, so a valid header travels with a mutated body)

use std::sync::Arc;

use zaino_header_chain::Record;
use zaino_primitives::types::{Block, BlockHash, Height, MerkleRoot, TransactionId};

/// Block that passed [`check_block`] (no other constructor)
#[derive(Debug, Clone)]
pub(crate) struct Checked(Arc<Block>);

impl Checked {
    pub(crate) fn block(&self) -> &Arc<Block> {
        &self.0
    }

    pub(crate) fn height(&self) -> Height {
        self.0.header().height
    }

    pub(crate) fn hash(&self) -> BlockHash {
        self.0.header().hash
    }
}

/// How a source misanswered a `(height, hash)` ask
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Misanswer {
    #[error("served block {got}")]
    WrongBlock { got: BlockHash },
    #[error("served a block whose coinbase says height {got:?}")]
    WrongHeight { got: Height },
    #[error("served a body whose txids do not rebuild the header's merkle root")]
    MerkleRoot,
    #[error("served a body repeating a txid pair (CVE-2012-2459)")]
    Mutated,
}

/// `block` = what was asked for at `height`, its body the one `record` commits to
pub(crate) fn check_block(
    block: Block,
    height: Height,
    record: &Record,
) -> Result<Checked, Misanswer> {
    let header = block.header();
    if header.hash != record.hash {
        return Err(Misanswer::WrongBlock { got: header.hash });
    }
    if header.height != height {
        return Err(Misanswer::WrongHeight { got: header.height });
    }
    let txids: Vec<TransactionId> = block.transactions().iter().map(|tx| tx.txid).collect();
    match MerkleRoot::of_txids(&txids) {
        None => Err(Misanswer::Mutated),
        Some(root) if root != record.merkle_root => Err(Misanswer::MerkleRoot),
        Some(_) => Ok(Checked(Arc::new(block))),
    }
}

#[cfg(test)]
mod tests {
    use zaino_header_chain::VerifiedChain;
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::Transaction;

    use super::*;

    /// Chain 0..=3 + a side 2': the asked block passes; a sibling, a block whose coinbase lies
    /// about its height, a body with a txid added (root moves) and a body repeating its last pair
    /// (root kept, CVE-2012-2459) are each refused by name
    #[test]
    fn only_the_asked_block_with_the_body_its_header_commits_to_passes() {
        let mut chain = Chain::new();
        let one = chain.mine(chain.genesis().hash);
        let paid: Vec<Transaction> = (0..3u8)
            .map(|n| {
                let mut tx = chain.block(chain.genesis().hash).transactions()[0].clone();
                tx.txid = [n + 1; 32].into();
                tx
            })
            .collect();
        let two = chain.mine_with(one.hash, paid);
        let tip = chain.mine(two.hash);
        let side = chain.mine(one.hash);
        let verified = VerifiedChain::regtest(&chain.path(tip.hash));
        let h2 = Height::try_from(2u32).expect("h");
        let record = verified.header_at(h2).expect("on the best chain");
        let block = |hash: BlockHash| chain.block(hash).clone();
        let with_txs = |txs: Vec<Transaction>| Block::new(block(two.hash).header().clone(), txs);
        let mut header = block(two.hash).header().clone();
        header.height = Height::try_from(9u32).expect("h");
        let lying = Block::new(header, block(two.hash).transactions().to_vec());
        let txs = block(two.hash).transactions().to_vec();
        let added = [txs.clone(), vec![txs[0].clone()]].concat();
        let repeated = [txs.clone(), vec![txs[2].clone()]].concat();

        let passed = check_block(block(two.hash), h2, &record).expect("the asked block");
        assert_eq!((passed.height(), passed.hash()), (h2, two.hash));
        let refused = [
            (block(side.hash), Misanswer::WrongBlock { got: side.hash }),
            (lying, Misanswer::WrongHeight { got: Height::try_from(9u32).expect("h") }),
            (with_txs(added), Misanswer::MerkleRoot),
            (with_txs(repeated), Misanswer::Mutated),
        ];
        for (served, why) in refused {
            assert_eq!(check_block(served, h2, &record).map(|c| c.hash()), Err(why));
        }
    }
}
