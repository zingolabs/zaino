//! One body per wanted height, from whichever member the balancer picks (`nfs.md` §6)
//!
//! - Checked = hash + coinbase height + merkle root vs the verified header (`verified-chain.md` §5)
//! - Misanswer → [`TrafficBalancer::report`] (sender benched) + asked again; who, hedges, retries =
//!   `zaino-traffic`'s

use std::sync::Arc;

use zaino_header_chain::Record;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, MerkleRoot, TransactionId};
use zaino_source::ChainDataSource;
use zaino_traffic::{TrafficBalancer, Urgency};

/// Block that passed [`check_block`] (no other constructor)
#[derive(Debug, Clone)]
pub(crate) struct Checked(Arc<Block>);

impl Checked {
    pub(crate) fn block(&self) -> &Arc<Block> {
        &self.0
    }

    pub(crate) fn at(&self) -> BlockRef {
        BlockRef { hash: self.0.header().hash, height: self.0.header().height }
    }
}

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

/// `block` = the one asked for at `height`, its body the one `record` commits to
///
/// - Mismatch = that member misanswered, never an invalid header (ZIP 256: a v5 hash does not
///   commit to authorizing data)
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
    match merkle_root(&block) {
        None => Err(Misanswer::Mutated),
        Some(root) if root != record.merkle_root => Err(Misanswer::MerkleRoot),
        Some(_) => Ok(Checked(Arc::new(block))),
    }
}

/// `None` = a repeated txid pair (CVE-2012-2459)
pub(crate) fn merkle_root(block: &Block) -> Option<MerkleRoot> {
    let txids: Vec<TransactionId> = block.transactions().iter().map(|tx| tx.txid).collect();
    MerkleRoot::of_txids(&txids)
}

/// Until a body passes [`check_block`] (drop = abandon: the balancer cancels its sends)
pub(crate) async fn fetch<S: ChainDataSource>(
    balancer: TrafficBalancer<S>,
    at: BlockRef,
    record: Record,
    urgency: Urgency,
) -> Checked {
    loop {
        let answered = balancer.block(at.hash, urgency).await;
        match check_block(answered.value, at.height, &record) {
            Ok(checked) => return checked,
            Err(why) => balancer.report(answered.ticket, &why),
        }
    }
}

#[cfg(test)]
mod tests {
    use zaino_header_chain::testing::HeaderViews;
    use zaino_header_chain::VerifiedChain;
    use zaino_primitives::testing::{h, Chain, MockChain};
    use zaino_primitives::types::Transaction;
    use zaino_source::testing::{Lie, MockValidator};
    use zaino_source::ChainDataSource;

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
        assert_eq!(passed.at(), BlockRef { hash: two.hash, height: h2 });
        let refused = [
            (block(side.hash), Misanswer::WrongBlock { got: side.hash }),
            (lying, Misanswer::WrongHeight { got: Height::try_from(9u32).expect("h") }),
            (with_txs(added), Misanswer::MerkleRoot),
            (with_txs(repeated), Misanswer::Mutated),
        ];
        for (served, why) in refused {
            assert_eq!(check_block(served, h2, &record).map(|c| c.at()), Err(why));
        }
    }

    /// M9: a `MockValidator` answers its best block honestly, and each `Lie` it is told to tell
    /// fails `check_block` by the rule that names it
    #[tokio::test]
    async fn every_lie_a_mock_validator_tells_fails_the_body_check() {
        let mut chain = MockChain::regtest();
        chain.mine_empty(1);
        let two = chain.mine(|b| b.tx(|t| t.txid([0x21; 32])).tx(|t| t.txid([0x22; 32])));
        let record = chain.verified(two).header_at(h(2)).expect("verified");
        let validator = MockValidator::following(&chain, two);
        let honest = validator.get_block_by_hash(two.hash).await.expect("on its best");
        let passed = check_block(honest, h(2), &record).map(|checked| checked.at());
        assert_eq!(passed, Ok(two));

        type Expected = fn(&Block) -> Misanswer;
        #[rustfmt::skip]
        let lies: [(Lie, Expected); 4] = [
            (Lie::WrongBlock,  |served| Misanswer::WrongBlock { got: served.header().hash }),
            (Lie::Poisoned,    |_| Misanswer::MerkleRoot),
            (Lie::Mutated,     |_| Misanswer::Mutated),
            (Lie::WrongHeight, |_| Misanswer::WrongHeight { got: h(3) }),
        ];
        for (lie, misanswer) in lies {
            validator.lie(Some(lie));
            let served = validator.get_block_by_hash(two.hash).await.expect("answers");
            let expected = misanswer(&served);
            let checked = check_block(served, h(2), &record).map(|checked| checked.at());
            assert_eq!(checked, Err(expected), "{lie:?}");
        }
    }
}
