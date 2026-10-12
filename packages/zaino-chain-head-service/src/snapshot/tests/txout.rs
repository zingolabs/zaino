//! The canonical window's txout delta, over a map-backed graph.

use zaino_chain_head::{ChainHeadBlock, ChainHeadError, ChainHeadTxOutSetService as _};
use zaino_primitives::types::{
    Block, BlockRef, ChainMetadata, Outpoint, RelativeChainWork, Script, Transaction,
    TransactionId, TransparentData, TransparentInput, TransparentOutput, TreeRoots, Zatoshis,
};

use crate::{
    graph::ChainGraph as _,
    snapshot::MapBackedSnapshot,
    tests::{block, height},
};

fn txid(id: u8) -> TransactionId {
    TransactionId::from([id; 32])
}

fn outpoint(id: u8, index: u32) -> Outpoint {
    Outpoint {
        txid: txid(id),
        index,
    }
}

fn output(value: u64) -> TransparentOutput {
    TransparentOutput {
        value: Zatoshis::new(value).expect("test value is in range"),
        script: Script::new(vec![0x51]),
    }
}

fn transaction(id: u8, spends: &[Outpoint], values: &[u64]) -> Transaction {
    Transaction {
        txid: txid(id),
        transparent: TransparentData {
            inputs: spends
                .iter()
                .map(|spent| TransparentInput {
                    prev_txid: spent.txid,
                    prev_index: spent.index,
                })
                .collect(),
            outputs: values.iter().copied().map(output).collect(),
        },
        sapling: Default::default(),
        orchard: Default::default(),
        ironwood: Default::default(),
    }
}

fn head_block(h: u32, transactions: Vec<Transaction>) -> ChainHeadBlock {
    let id = u16::try_from(h).expect("test height fits an id");
    let header = block(h, id, id - 1).header;
    let block =
        Block::try_new(header, transactions, ChainMetadata::ZERO).expect("a test block has a tx");
    ChainHeadBlock {
        reference: BlockRef {
            hash: block.header.hash,
            height: block.header.height,
        },
        parent_hash: block.header.prev_hash,
        work: RelativeChainWork::ZERO,
        block,
        tree_roots: TreeRoots {
            sapling: None,
            orchard: None,
            ironwood: None,
        },
    }
}

/// Window 10..=12. Transaction 1 creates two outputs; 2 spends one of them and
/// an output from below the window; 3 spends 2's only output.
fn window() -> MapBackedSnapshot {
    let mut graph =
        MapBackedSnapshot::from_initial_block(head_block(10, vec![transaction(1, &[], &[5, 7])]));
    graph
        .extend(head_block(
            11,
            vec![transaction(2, &[outpoint(1, 0), outpoint(9, 3)], &[4])],
        ))
        .expect("11 extends 10");
    graph
        .extend(head_block(
            12,
            vec![transaction(3, &[outpoint(2, 0)], &[3])],
        ))
        .expect("12 extends 11");
    graph
}

#[test]
fn spends_inside_the_range_cancel_and_spends_from_below_are_reported() {
    let delta = window().txout_delta(height(10)).expect("inside the window");
    assert_eq!(
        delta
            .created
            .iter()
            .map(|created| created.outpoint)
            .collect::<Vec<_>>(),
        vec![outpoint(1, 1), outpoint(3, 0)]
    );
    assert_eq!(delta.spent_below, vec![outpoint(9, 3)]);
}

#[test]
fn outputs_below_the_start_are_spends_from_below() {
    let delta = window().txout_delta(height(11)).expect("inside the window");
    assert_eq!(delta.created.len(), 1);
    assert_eq!(delta.created[0].outpoint, outpoint(3, 0));
    assert_eq!(delta.created[0].output, output(3));
    assert_eq!(delta.spent_below, vec![outpoint(1, 0), outpoint(9, 3)]);
}

#[test]
fn a_start_above_the_tip_is_empty() {
    let delta = window().txout_delta(height(13)).expect("above the tip");
    assert!(delta.created.is_empty());
    assert!(delta.spent_below.is_empty());
}

#[test]
fn a_start_below_the_window_is_refused() {
    assert_eq!(
        window().txout_delta(height(9)),
        Err(ChainHeadError::BelowWindow {
            start: height(9),
            floor: height(10),
        })
    );
}
