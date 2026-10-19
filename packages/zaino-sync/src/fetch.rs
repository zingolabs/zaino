//! One body per wanted block, from whichever member the balancer picks
//!
//! - By hash (NFS, peers or trusted): hash + merkle root vs the verified header + coinbase height
//! - By height (FinalizedFollower, trusted members only, batched): coinbase height alone, body
//!   trusted (linkage = the follower's, in delivery order)
//! - Misanswer → [`TrafficBalancer::report`] (sender benched) + asked again
//! - who, hedges, retries = `zaino-traffic`'s

use std::sync::Arc;

use zaino_header_chain::Record;
use zaino_primitives::types::{Block, BlockRef, BodyMismatch};
use zaino_source::ChainDataSource;
use zaino_traffic::{TrafficBalancer, Urgency};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

/// Block that passed `check_block` or `check_block_at` (no other constructor)
#[derive(Debug, Clone)]
pub struct Checked(Arc<Block>);

impl Checked {
    pub fn block(&self) -> &Arc<Block> {
        &self.0
    }

    pub fn block_ref(&self) -> BlockRef {
        self.0.block_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Misanswer {
    #[error("served block {got}")]
    WrongBlock { got: BlockHash },
    #[error("served a block whose coinbase says height {got:?}")]
    WrongHeight { got: BlockHeight },
    #[error("served {0}")]
    Body(#[from] BodyMismatch),
    #[error("served {got} blocks for {asked} heights")]
    Count { asked: usize, got: usize },
}

/// `block` = the one asked for at `height` (hash = `record`'s: its header = the verified one)
///
/// - Mismatch = that member misanswered, never an invalid header (ZIP 256: a v5 hash does not
///   commit to authorizing data)
pub(crate) fn check_block(
    block: Block,
    height: BlockHeight,
    record: &Record,
) -> Result<Checked, Misanswer> {
    let header = block.header();
    if header.hash != record.hash {
        return Err(Misanswer::WrongBlock { got: header.hash });
    }
    block.check_body()?;
    check_block_at(block, height)
}

/// `block` = the one at `height` (a trusted member's by-height answer)
///
/// - body trusted, not re-hashed
/// - coinbase height checked (batch paired with `heights` by position)
fn check_block_at(block: Block, height: BlockHeight) -> Result<Checked, Misanswer> {
    let labelled = block.header().height;
    if labelled != height {
        return Err(Misanswer::WrongHeight { got: labelled });
    }
    Ok(Checked(Arc::new(block)))
}

/// Until a body passes `check_block` (drop = abandon: the balancer cancels its sends)
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

/// Until one trusted member's batch at `heights` passes [`check_block_at`] block by block
/// (`heights` order, drop = abandon)
pub(crate) async fn fetch_at<S: ChainDataSource>(
    balancer: TrafficBalancer<S>,
    heights: Arc<[BlockHeight]>,
    urgency: Urgency,
) -> Vec<Checked> {
    loop {
        let answered = balancer.blocks_at(Arc::clone(&heights), urgency).await;
        match check_blocks_at(answered.value, &heights) {
            Ok(checked) => return checked,
            Err(why) => balancer.report(answered.ticket, &why),
        }
    }
}

/// One block per `heights` entry, in order, each passing [`check_block_at`]
fn check_blocks_at(blocks: Vec<Block>, heights: &[BlockHeight]) -> Result<Vec<Checked>, Misanswer> {
    if blocks.len() != heights.len() {
        return Err(Misanswer::Count { asked: heights.len(), got: blocks.len() });
    }
    blocks.into_iter().zip(heights).map(|(block, height)| check_block_at(block, *height)).collect()
}

#[cfg(test)]
mod tests {
    use zaino_header_chain::testing::HeaderViews;
    use zaino_primitives::testing::{h, MockChain};
    use zaino_primitives::types::Transaction;
    use zaino_source::testing::{Lie, ValidatorViews};
    use zaino_source::ChainDataSource;

    use super::*;

    /// Chain 0..=3 (2 = coinbase + 2 txs) + a side 2'
    ///
    /// - asked block passes
    /// - by hash: sibling, header lying about its height, txid added (root moves), last pair
    ///   repeated (root kept, CVE-2012-2459) → each refused by name
    /// - by height (trusted member's body, trusted): only the height lie refused (sibling = the
    ///   follower's linkage to refuse)
    /// - batch short of its heights = `Count`, never truncated
    #[test]
    fn only_the_asked_block_with_the_body_its_header_commits_to_passes() {
        let mut chain = MockChain::regtest();
        chain.mine_empty(1);
        let two = chain.mine(|b| b.tx(|t| t.txid([1; 32])).tx(|t| t.txid([2; 32])));
        let tip = chain.mine_empty(1);
        let side = chain.fork(h(1)).mine_empty(1).tip();
        let h2 = h(2);
        let record = chain.snapshot(tip).header_at(h2).expect("on the best chain");
        let block = |hash: BlockHash| Block::clone(chain.block(hash));
        let with_txs = |txs: Vec<Transaction>| Block::new(block(two.hash).header().clone(), txs);
        let mut header = block(two.hash).header().clone();
        header.height = BlockHeight::from(9);
        let lying = Block::new(header, block(two.hash).transactions().to_vec());
        let txs = block(two.hash).transactions().to_vec();
        let added = [txs.clone(), vec![txs[0].clone()]].concat();
        let repeated = [txs.clone(), vec![txs[2].clone()]].concat();

        let passed = check_block(block(two.hash), h2, &record).expect("the asked block");
        assert_eq!(passed.block_ref(), BlockRef { hash: two.hash, height: h2 });
        let passed = check_block_at(block(two.hash), h2).expect("its own header's body");
        assert_eq!(passed.block_ref(), BlockRef { hash: two.hash, height: h2 });
        let (side_at, two_at) = (BlockRef { hash: side.hash, height: h2 }, two);
        let wrong_height = Misanswer::WrongHeight { got: BlockHeight::from(9) };
        let merkle_root = Misanswer::Body(BodyMismatch::MerkleRoot);
        let mutated = Misanswer::Body(BodyMismatch::Mutated);
        #[rustfmt::skip]
        let served = [
            (block(side.hash),   Err(Misanswer::WrongBlock { got: side.hash }), Ok(side_at)),
            (lying,              Err(wrong_height),                             Err(wrong_height)),
            (with_txs(added),    Err(merkle_root),                              Ok(two_at)),
            (with_txs(repeated), Err(mutated),                                  Ok(two_at)),
        ];
        for (block, by_hash, by_height) in served {
            let checked = |checked: Result<Checked, Misanswer>| checked.map(|c| c.block_ref());
            let got = (
                checked(check_block(block.clone(), h2, &record)),
                checked(check_block_at(block, h2)),
            );
            assert_eq!(got, (by_hash, by_height));
        }

        let short = check_blocks_at(vec![block(two.hash)], &[h2, h(3)]).map(|_| ());
        assert_eq!(short, Err(Misanswer::Count { asked: 2, got: 1 }), "a short batch = misanswer");
    }

    /// M9: `MockValidator` answers its best block honestly, by hash and by height
    ///
    /// - each `Lie` it is told to tell fails the body check by the rule that names it
    /// - by height (trusted member): only the height lie, re-mined block = successor's link to
    ///   refuse
    #[tokio::test]
    async fn every_lie_a_mock_validator_tells_fails_the_body_check() {
        let mut chain = MockChain::regtest();
        chain.mine_empty(1);
        let two = chain.mine(|b| b.tx(|t| t.txid([0x21; 32])).tx(|t| t.txid([0x22; 32])));
        let record = chain.snapshot(two).header_at(h(2)).expect("verified");
        let validator = chain.validator(two);
        let honest = validator.get_block_by_hash(two.hash).await.expect("on its best");
        let passed = check_block(honest, h(2), &record).map(|checked| checked.block_ref());
        assert_eq!(passed, Ok(two));
        let honest = validator.get_blocks_by_height(&[h(2)]).await.expect("on its best").remove(0);
        assert_eq!(check_block_at(honest, h(2)).map(|checked| checked.block_ref()), Ok(two));

        type Expected = fn(&Block) -> Option<Misanswer>;
        #[rustfmt::skip]
        let lies: [(Lie, Expected, Expected); 4] = [
            (Lie::WrongBlock,  |served| Some(Misanswer::WrongBlock { got: served.header().hash }),
                               |_| None),
            (Lie::Poisoned,    |_| Some(Misanswer::Body(BodyMismatch::MerkleRoot)),
                               |_| None),
            (Lie::Mutated,     |_| Some(Misanswer::Body(BodyMismatch::Mutated)),
                               |_| None),
            (Lie::WrongHeight, |_| Some(Misanswer::WrongHeight { got: h(3) }),
                               |_| Some(Misanswer::WrongHeight { got: h(3) })),
        ];
        for (lie, by_hash, by_height) in lies {
            validator.lie(Some(lie));
            let served = validator.get_block_by_hash(two.hash).await.expect("answers");
            let expected = by_hash(&served);
            let checked = check_block(served, h(2), &record).err();
            assert_eq!(checked, expected, "{lie:?} by hash");
            let served = validator.get_blocks_by_height(&[h(2)]).await.expect("answers").remove(0);
            let expected = by_height(&served);
            assert_eq!(check_block_at(served, h(2)).err(), expected, "{lie:?} by height");
        }
    }
}
