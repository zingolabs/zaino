//! Sample blocks for this crate's tests and for consumers testing against a real index
//!
//! - behind the `testing` feature (never compiled into a served binary)

use std::num::NonZeroUsize;

use zaino_persistence::{SequenceRead, Store, Tiered, TieredView};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{
    Block, BlockFees, CompactCiphertext, Fee, Height, OrchardAction, OrchardData, OutPoint,
    SaplingData, SaplingOutput, SaplingSpend, Script, Transaction, TransparentData,
    TransparentOutput, Zatoshis,
};

use crate::{fold, CompactBlockReader, HASH};

/// [`block`]`(0..count)` folded and committed to `store` (one commit), as the index serves them
pub fn committed<S: Store<View: SequenceRead>>(
    store: S,
    count: u32,
) -> CompactBlockReader<TieredView<S::View>> {
    let network = store.schema().network;
    let mut tiered = Tiered::new(store, NonZeroUsize::MAX);
    for height in 0..count {
        let (block, fees) = block(height);
        let parent = CompactBlockReader::new(tiered.view(), network);
        let changes = fold(&parent, &block, &fees).expect("one sample tx per block: far below u32");
        assert!(!tiered.stage(changes, 0), "a batch of usize::MAX bytes never fills");
    }
    if let Some(last) = count.checked_sub(1) {
        tiered.finalize(Height::try_from(last).expect("a small height"));
    }
    CompactBlockReader::new(tiered.view(), network)
}

fn bytes32(seed: u8) -> [u8; HASH] {
    [seed; HASH]
}

fn ciphertext(seed: u8) -> CompactCiphertext {
    [seed; CompactCiphertext::LENGTH].into()
}

fn action(seed: u8) -> OrchardAction {
    OrchardAction {
        nullifier: bytes32(seed).into(),
        cmx: bytes32(seed.wrapping_add(1)).into(),
        ephemeral_key: bytes32(seed.wrapping_add(2)).into(),
        enc_ciphertext: ciphertext(seed.wrapping_add(3)),
    }
}

/// Block at `height` carrying every pool (dropped pool = missing field) + its fees
///
/// - one deterministic `testing::Chain`, every block the same tx: `block(h)` links onto
///   `block(h − 1)`; tree sizes after `h` = (h + 1) × 1 / 1 / 2
/// - its one tx priced at 5 000 zat (value-balance's job to derive, stated here)
pub fn block(height: u32) -> (Block, BlockFees) {
    let tx = Transaction {
        txid: bytes32(0x11).into(),
        transparent: TransparentData {
            coinbase: false,
            inputs: vec![OutPoint { txid: bytes32(0x22).into(), vout: 7 }],
            outputs: vec![TransparentOutput {
                value: Zatoshis::new(12_345).expect("in range"),
                script: Script::new(vec![0x76, 0xa9, 0x14]),
            }],
        },
        sprout: Default::default(),
        sapling: SaplingData {
            spends: vec![SaplingSpend { nullifier: bytes32(0x33).into() }],
            outputs: vec![SaplingOutput {
                cmu: bytes32(0x44).into(),
                ephemeral_key: bytes32(0x55).into(),
                enc_ciphertext: ciphertext(0x66),
            }],
            ..Default::default()
        },
        orchard: OrchardData { actions: vec![action(0x77)], ..Default::default() },
        ironwood: OrchardData { actions: vec![action(0x88), action(0x99)], ..Default::default() },
    };
    let mut chain = Chain::with_genesis(vec![tx.clone()]);
    let mut tip = chain.genesis();
    for _ in 0..height {
        tip = chain.mine_with(tip.hash, vec![tx.clone()]);
    }
    let block = chain.block(tip.hash).clone();
    let fees = BlockFees {
        height: block.header().height,
        hash: block.header().hash,
        fees: vec![Fee::Paid(Zatoshis::new(5_000).expect("in range"))],
    };
    (block, fees)
}
