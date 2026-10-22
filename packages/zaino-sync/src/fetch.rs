//! One body per wanted block, from whichever member the balancer picks
//!
//! - By hash (NFS, `GetTransaction`, peers or trusted): hash + merkle root vs a header we hold
//!   (or a block our indexes folded) + coinbase height
//! - By height (FinalizedFollower, trusted members only, batched): coinbase height alone, body
//!   trusted (linkage = the follower's, in delivery order)
//! - Misanswer → [`TrafficBalancer::report`] (sender benched) + asked again
//! - who, hedges, retries = `zaino-traffic`'s

use std::sync::Arc;

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

/// `block` = the one asked for at `at` (its hash = a header we hold, or one our indexes folded)
///
/// - Mismatch = that member misanswered, never an invalid header (ZIP 256: a v5 hash does not
///   commit to authorizing data)
pub(crate) fn check_block(block: Block, at: BlockRef) -> Result<Checked, Misanswer> {
    let header = block.header();
    if header.hash != at.hash {
        return Err(Misanswer::WrongBlock { got: header.hash });
    }
    block.check_body()?;
    check_block_at(block, at.height)
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
    urgency: Urgency,
) -> Checked {
    loop {
        let answered = balancer.block(at.hash, urgency).await;
        match check_block(answered.value, at) {
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
    use zaino_primitives::types::{DecodedHeader, Transaction};
    use zaino_source::testing::{h, Lie, MockChain};
    use zaino_source::ChainDataSource;

    use super::*;

    /// Asked for A2 (coinbase + 2 txs) by hash and by height
    ///
    /// - by hash: only A2 with the body its header commits to passes, each fault refused by name
    /// - by height (trusted member's body, trusted): only the height lie refused (B2 = the
    ///   follower's linkage to refuse)
    #[test]
    fn only_the_asked_block_with_the_body_its_header_commits_to_passes() {
        let mut chain = MockChain::regtest();
        chain.mine_empty(1);
        let a2 = chain.mine(|b| b.tx(|t| t).tx(|t| t));
        let b2 = chain.fork(h(1)).mine_empty(1).tip();
        let block = |hash: BlockHash| Block::clone(chain.block(hash));
        let a2_header = block(a2.hash).header().clone();
        let a2_txs = block(a2.hash).transactions().to_vec();
        let with_txs = |txs: Vec<Transaction>| Block::new(a2_header.clone(), txs);
        let lied_height = h(9);
        let lying_height =
            Block::new(DecodedHeader { height: lied_height, ..a2_header.clone() }, a2_txs.clone());
        let txid_added = [a2_txs.clone(), vec![a2_txs[0].clone()]].concat();
        let last_pair_repeated = [a2_txs.clone(), vec![a2_txs[2].clone()]].concat();

        struct Served {
            case: &'static str,
            block: Block,
            by_hash: Result<BlockRef, Misanswer>,
            by_height: Result<BlockRef, Misanswer>,
        }
        let b2_at_2 = BlockRef { hash: b2.hash, height: h(2) };
        let wrong_height = Misanswer::WrongHeight { got: lied_height };
        let cases = [
            Served { case: "A2", block: block(a2.hash), by_hash: Ok(a2), by_height: Ok(a2) },
            Served {
                case: "sibling B2",
                block: block(b2.hash),
                by_hash: Err(Misanswer::WrongBlock { got: b2.hash }),
                by_height: Ok(b2_at_2),
            },
            Served {
                case: "header lying its height is 9",
                block: lying_height,
                by_hash: Err(wrong_height),
                by_height: Err(wrong_height),
            },
            Served {
                case: "txid added (root moves)",
                block: with_txs(txid_added),
                by_hash: Err(Misanswer::Body(BodyMismatch::MerkleRoot)),
                by_height: Ok(a2),
            },
            Served {
                case: "last pair repeated (root kept, CVE-2012-2459)",
                block: with_txs(last_pair_repeated),
                by_hash: Err(Misanswer::Body(BodyMismatch::Mutated)),
                by_height: Ok(a2),
            },
        ];
        for Served { case, block, by_hash, by_height } in cases {
            let by_hash_got = check_block(block.clone(), a2).map(|checked| checked.block_ref());
            assert_eq!(by_hash_got, by_hash, "{case} asked by A2's hash");
            let by_height_got = check_block_at(block, h(2)).map(|checked| checked.block_ref());
            assert_eq!(by_height_got, by_height, "{case} asked at height 2");
        }
    }

    #[test]
    fn a_batch_short_of_its_heights_is_a_misanswer_never_truncated() {
        let mut chain = MockChain::regtest();
        let one = chain.mine_empty(1);
        let only_one = vec![Block::clone(chain.block(one.hash))];
        let short = check_blocks_at(only_one, &[h(1), h(2)]).map(|checked| checked.len());
        assert_eq!(short, Err(Misanswer::Count { asked: 2, got: 1 }), "1 block for heights 1, 2");
    }

    /// `MockValidator`'s lies (M9) over the client: honest answer passes, each `Lie` fails the
    /// body check by the rule that names it
    ///
    /// - by height (trusted member): only the height lie (re-mined block = successor's link to
    ///   refuse)
    #[tokio::test]
    async fn every_lie_a_mock_validator_tells_fails_the_body_check() {
        let mut chain = MockChain::regtest();
        chain.mine_empty(1);
        let two = chain.mine(|b| b.tx(|t| t).tx(|t| t));
        let validator = chain.validator(two);
        let client = validator.client();
        let honest = client.get_block_by_hash(two.hash).await.expect("on its best");
        let passed = check_block(honest, two).map(|checked| checked.block_ref());
        assert_eq!(passed, Ok(two), "honest answer by hash");
        let honest = client.get_blocks_by_height(&[h(2)]).await.expect("on its best").remove(0);
        let passed = check_block_at(honest, h(2)).map(|checked| checked.block_ref());
        assert_eq!(passed, Ok(two), "honest answer at height 2");

        struct Told {
            lie: Lie,
            by_hash: fn(served: &Block) -> Option<Misanswer>,
            by_height: Option<Misanswer>,
        }
        let lies = [
            Told {
                lie: Lie::WrongBlock,
                by_hash: |served| Some(Misanswer::WrongBlock { got: served.header().hash }),
                by_height: None,
            },
            Told {
                lie: Lie::Poisoned,
                by_hash: |_| Some(Misanswer::Body(BodyMismatch::MerkleRoot)),
                by_height: None,
            },
            Told {
                lie: Lie::Mutated,
                by_hash: |_| Some(Misanswer::Body(BodyMismatch::Mutated)),
                by_height: None,
            },
            Told {
                lie: Lie::WrongHeight,
                by_hash: |_| Some(Misanswer::WrongHeight { got: h(3) }),
                by_height: Some(Misanswer::WrongHeight { got: h(3) }),
            },
        ];
        for Told { lie, by_hash, by_height } in lies {
            validator.lie(Some(lie));
            let served = client.get_block_by_hash(two.hash).await.expect("answers");
            let expected = by_hash(&served);
            assert_eq!(check_block(served, two).err(), expected, "{lie:?} asked by hash");
            let served = client.get_blocks_by_height(&[h(2)]).await.expect("answers").remove(0);
            assert_eq!(check_block_at(served, h(2)).err(), by_height, "{lie:?} asked at height 2");
        }
    }
}
