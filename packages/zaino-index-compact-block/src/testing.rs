//! Sample blocks for this crate's tests and for consumers testing against a real index
//!
//! - behind the `testing` feature (never compiled into a served binary)

use zaino_persistence::{SequenceRead, Store};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{
    Block, BlockFees, CompactCiphertext, Fee, OrchardAction, OrchardData, OutPoint, SaplingData,
    SaplingOutput, SaplingSpend, Script, Transaction, TransparentData, TransparentOutput, Zatoshis,
};

use crate::{fold, CompactBlockReader, HASH};

/// `store` with [`block`]`(0..count)` folded and committed (one commit), as the index holds them
pub fn committed<S: Store<View: SequenceRead>>(mut store: S, count: u32) -> S {
    for height in 0..count {
        let (block, fees) = block(height);
        let mut changes = store.changes(block.at());
        let parent = CompactBlockReader::new(store.staged());
        fold(&parent, &block, &fees, &mut changes).expect("one sample tx per block: far below u32");
        store.apply(changes);
    }
    store.commit().unwrap_or_else(|error| error.commit_failed("compact_block", store.path()));
    store
}

/// [`block`]`(0..count)`, genesis first (a `VerifiedChain::regtest` path)
pub fn chain(count: u32) -> Vec<Block> {
    (0..count).map(|height| block(height).0).collect()
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
