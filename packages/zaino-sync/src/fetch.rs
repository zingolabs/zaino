//! One body per wanted block, from whichever member the balancer picks
//!
//! - By hash (NFS): hash + coinbase height + merkle root vs the verified header
//! - By height (FinalFollower, trusted members only): coinbase height + merkle root vs its own
//!   header (linkage = the follower's, in delivery order)
//! - Misanswer → [`TrafficBalancer::report`] (sender benched) + asked again; who, hedges, retries =
//!   `zaino-traffic`'s

use std::sync::Arc;

use zaino_header_chain::Record;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, MerkleRoot, TransactionId};
use zaino_source::ChainDataSource;
use zaino_traffic::{TrafficBalancer, Urgency};

/// Block that passed [`check_block`] (no other constructor)
#[derive(Debug, Clone)]
pub struct Checked(Arc<Block>);

impl Checked {
    pub fn block(&self) -> &Arc<Block> {
        &self.0
    }

    pub fn at(&self) -> BlockRef {
        BlockRef { hash: self.0.header().hash, height: self.0.header().height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Misanswer {
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
pub fn check_block(block: Block, height: Height, record: &Record) -> Result<Checked, Misanswer> {
    let header = block.header();
    if header.hash != record.hash {
        return Err(Misanswer::WrongBlock { got: header.hash });
    }
    check_body(block, height, record.merkle_root)
}

/// `block` = one at `height` whose body its own header commits to (a by-height answer: no
/// verified header to hold it to)
pub fn check_block_at(block: Block, height: Height) -> Result<Checked, Misanswer> {
    let committed = block.header().merkle_root;
    check_body(block, height, committed)
}

/// Coinbase height + body vs `committed` (CVE-2012-2459 mutation refused)
fn check_body(block: Block, height: Height, committed: MerkleRoot) -> Result<Checked, Misanswer> {
    let labelled = block.header().height;
    if labelled != height {
        return Err(Misanswer::WrongHeight { got: labelled });
    }
    match merkle_root(&block) {
        None => Err(Misanswer::Mutated),
        Some(root) if root != committed => Err(Misanswer::MerkleRoot),
        Some(_) => Ok(Checked(Arc::new(block))),
    }
}

/// `None` = a repeated txid pair (CVE-2012-2459)
pub fn merkle_root(block: &Block) -> Option<MerkleRoot> {
    let txids: Vec<TransactionId> = block.transactions().iter().map(|tx| tx.txid).collect();
    MerkleRoot::of_txids(&txids)
}

/// Until a body passes [`check_block`] (drop = abandon: the balancer cancels its sends)
pub async fn fetch<S: ChainDataSource>(
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

/// Until a trusted member's body at `height` passes [`check_block_at`] (drop = abandon)
pub async fn fetch_at<S: ChainDataSource>(
    balancer: TrafficBalancer<S>,
    height: Height,
    urgency: Urgency,
) -> Checked {
    loop {
        let answered = balancer.block_at(height, urgency).await;
        match check_block_at(answered.value, height) {
            Ok(checked) => return checked,
            Err(why) => balancer.report(answered.ticket, &why),
        }
    }
}

#[cfg(test)]
mod tests {
    use zaino_header_chain::testing::HeaderViews;
    use zaino_primitives::testing::{h, MockChain};
    use zaino_primitives::types::Transaction;
    use zaino_source::testing::{Lie, MockValidator};
    use zaino_source::ChainDataSource;

    use super::*;

    /// Chain 0..=3 (2 = coinbase + 2 txs) + a side 2': the asked block passes; a sibling, a block
    /// whose header lies about its height, a body with a txid added (root moves) and a body
    /// repeating its last pair (root kept, CVE-2012-2459) are each refused by name; by height the
    /// same, except the sibling passes (no verified hash: the follower's linkage refuses it)
    #[test]
    fn only_the_asked_block_with_the_body_its_header_commits_to_passes() {
        let mut chain = MockChain::regtest();
        chain.mine_empty(1);
        let two = chain.mine(|b| b.tx(|t| t.txid([1; 32])).tx(|t| t.txid([2; 32])));
        let tip = chain.mine_empty(1);
        let side = chain.fork(h(1)).mine_empty(1).tip();
        let h2 = h(2);
        let record = chain.verified(tip).header_at(h2).expect("on the best chain");
        let block = |hash: BlockHash| Block::clone(chain.block(hash));
        let with_txs = |txs: Vec<Transaction>| Block::new(block(two.hash).header().clone(), txs);
        let mut header = block(two.hash).header().clone();
        header.height = Height::try_from(9u32).expect("h");
        let lying = Block::new(header, block(two.hash).transactions().to_vec());
        let txs = block(two.hash).transactions().to_vec();
        let added = [txs.clone(), vec![txs[0].clone()]].concat();
        let repeated = [txs.clone(), vec![txs[2].clone()]].concat();

        let passed = check_block(block(two.hash), h2, &record).expect("the asked block");
        assert_eq!(passed.at(), BlockRef { hash: two.hash, height: h2 });
        let passed = check_block_at(block(two.hash), h2).expect("its own header's body");
        assert_eq!(passed.at(), BlockRef { hash: two.hash, height: h2 });
        let side_at = BlockRef { hash: side.hash, height: h2 };
        let wrong_height = Misanswer::WrongHeight { got: Height::try_from(9u32).expect("h") };
        #[rustfmt::skip]
        let served = [
            (block(side.hash),   Err(Misanswer::WrongBlock { got: side.hash }), Ok(side_at)),
            (lying,              Err(wrong_height),                             Err(wrong_height)),
            (with_txs(added),    Err(Misanswer::MerkleRoot),                    Err(Misanswer::MerkleRoot)),
            (with_txs(repeated), Err(Misanswer::Mutated),                       Err(Misanswer::Mutated)),
        ];
        for (block, by_hash, by_height) in served {
            let checked = |checked: Result<Checked, Misanswer>| checked.map(|c| c.at());
            let got = (
                checked(check_block(block.clone(), h2, &record)),
                checked(check_block_at(block, h2)),
            );
            assert_eq!(got, (by_hash, by_height));
        }
    }

    /// M9: a `MockValidator` answers its best block honestly, by hash and by height, and each
    /// `Lie` it is told to tell fails the body check by the rule that names it (by height: a
    /// re-mined block passes, its successor's link refuses it)
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
        let honest = validator.get_block_by_height(h(2)).await.expect("on its best");
        assert_eq!(check_block_at(honest, h(2)).map(|checked| checked.at()), Ok(two));

        type Expected = fn(&Block) -> Option<Misanswer>;
        #[rustfmt::skip]
        let lies: [(Lie, Expected, Expected); 4] = [
            (Lie::WrongBlock,  |served| Some(Misanswer::WrongBlock { got: served.header().hash }),
                               |_| None),
            (Lie::Poisoned,    |_| Some(Misanswer::MerkleRoot), |_| Some(Misanswer::MerkleRoot)),
            (Lie::Mutated,     |_| Some(Misanswer::Mutated),    |_| Some(Misanswer::Mutated)),
            (Lie::WrongHeight, |_| Some(Misanswer::WrongHeight { got: h(3) }),
                               |_| Some(Misanswer::WrongHeight { got: h(3) })),
        ];
        for (lie, by_hash, by_height) in lies {
            validator.lie(Some(lie));
            let served = validator.get_block_by_hash(two.hash).await.expect("answers");
            let expected = by_hash(&served);
            let checked = check_block(served, h(2), &record).err();
            assert_eq!(checked, expected, "{lie:?} by hash");
            let served = validator.get_block_by_height(h(2)).await.expect("answers");
            let expected = by_height(&served);
            assert_eq!(check_block_at(served, h(2)).err(), expected, "{lie:?} by height");
        }
    }
}
