//! The txout set at the tip: the store's accumulator extended by the window.
//!
//! The reference is the fake store folded from scratch over the whole chain, so
//! each case asserts the composition equals the set it claims to describe.

use std::sync::Arc;

use zaino_chain::testing::{block_at, txid, Chain, FakeHead, FakeSource, FakeStore};
use zaino_chain::{ChainViewComposer, ChainViewError, TxOutSetRead as _};
use zaino_chain_store::TxOutSetIndex as _;
use zaino_primitives::types::{
    Block, ChainMetadata, Outpoint, Script, Transaction, TransparentData, TransparentInput,
    TransparentOutput, Zatoshis,
};

const TIP: u32 = 29;

fn p2pkh(tag: u8) -> Script {
    let mut script = vec![0x76, 0xa9, 0x14];
    script.extend_from_slice(&[tag; 20]);
    script.extend_from_slice(&[0x88, 0xac]);
    Script::new(script)
}

fn p2sh(tag: u8) -> Script {
    let mut script = vec![0xa9, 0x14];
    script.extend_from_slice(&[tag; 20]);
    script.push(0x87);
    Script::new(script)
}

fn op_return() -> Script {
    Script::new(vec![0x6a, 0x01, 0x00])
}

fn output(script: Script, value: u64) -> TransparentOutput {
    TransparentOutput {
        value: Zatoshis::new(value).expect("test value is in range"),
        script,
    }
}

fn outpoint(tag: u8, index: u32) -> Outpoint {
    Outpoint {
        txid: txid(tag),
        index,
    }
}

fn transaction(tag: u8, spends: &[Outpoint], outputs: Vec<TransparentOutput>) -> Transaction {
    Transaction {
        txid: txid(tag),
        transparent: TransparentData {
            inputs: spends
                .iter()
                .map(|spent| TransparentInput {
                    prev_txid: spent.txid,
                    prev_index: spent.index,
                })
                .collect(),
            outputs,
        },
        sapling: Default::default(),
        orchard: Default::default(),
        ironwood: Default::default(),
    }
}

/// Spends within the store, within the window, and across the seam, plus an
/// unspendable output that must count for nothing.
fn activity(h: u32) -> Vec<Transaction> {
    match h {
        2 => vec![transaction(
            0xa0,
            &[],
            vec![
                output(p2pkh(1), 100),
                output(p2pkh(2), 50),
                output(op_return(), 0),
            ],
        )],
        3 => vec![transaction(
            0xa1,
            &[],
            vec![output(p2pkh(9), 11), output(p2pkh(10), 12)],
        )],
        5 => vec![transaction(0xb0, &[], vec![output(p2sh(3), 70)])],
        12 => vec![transaction(
            0xc0,
            &[outpoint(0xa0, 0)],
            vec![output(p2pkh(4), 90)],
        )],
        17 => vec![transaction(
            0xd0,
            &[outpoint(0xa0, 1)],
            vec![output(p2pkh(5), 40)],
        )],
        21 => vec![transaction(
            0xe0,
            &[outpoint(0xb0, 0)],
            vec![output(p2pkh(6), 60), output(p2pkh(7), 5)],
        )],
        22 => vec![transaction(0xe1, &[outpoint(0xa1, 0)], Vec::new())],
        24 => vec![transaction(
            0xf0,
            &[outpoint(0xe0, 1), outpoint(0xd0, 0)],
            vec![output(p2pkh(8), 30)],
        )],
        26 => vec![transaction(0xf1, &[outpoint(0xc0, 0)], Vec::new())],
        _ => Vec::new(),
    }
}

fn chain() -> Chain {
    Chain::from_blocks(
        (0..=TIP)
            .map(|h| {
                let base = block_at(h);
                let mut transactions = base.transactions().to_vec();
                transactions.extend(activity(h));
                Block::try_new(base.header.clone(), transactions, ChainMetadata::ZERO)
                    .expect("a test block keeps its coinbase")
            })
            .collect(),
    )
}

fn view(
    chain: &Chain,
    store_top: u32,
    floor: u32,
) -> ChainViewComposer<FakeStore, FakeHead, FakeSource> {
    ChainViewComposer::builder(
        FakeStore::covering(chain, store_top),
        FakeHead::covering(chain, floor, TIP),
        Arc::new(FakeSource::over(chain)),
    )
    .serving_txout_set()
    .build()
}

async fn whole_chain(chain: &Chain) -> zaino_chain_store::TxOutSetAccumulator {
    FakeStore::covering(chain, TIP)
        .txout_set()
        .await
        .expect("the reference fold succeeds")
}

#[tokio::test]
async fn the_set_at_the_tip_matches_a_fold_over_the_whole_chain() {
    let chain = chain();
    let expected = whole_chain(&chain).await;
    // a1:1, e0:0 and f0:0 survive; so do their three transactions.
    assert_eq!(expected.transaction_outputs, 3);
    assert_eq!(expected.transactions, 3);

    // Overlapping, abutting, and a store already at the tip.
    for (store_top, floor) in [(19, 15), (19, 20), (TIP, 20)] {
        let composed = view(&chain, store_top, floor)
            .snapshot()
            .txout_set()
            .await
            .expect("contiguous coverage is serviceable");
        assert_eq!(
            composed, expected,
            "store to {store_top}, window from {floor}"
        );
    }
}

#[tokio::test]
async fn a_hole_between_store_and_window_is_not_serviceable() {
    let chain = chain();
    assert!(matches!(
        view(&chain, 10, 20).snapshot().txout_set().await,
        Err(ChainViewError::NotServiceable(_))
    ));
}
