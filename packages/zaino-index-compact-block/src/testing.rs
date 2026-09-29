//! Sample blocks for this crate's tests and for consumers testing against a real index
//!
//! - behind the `testing` feature (never compiled into a served binary)

use zaino_primitives::types::{
    Block, BlockFees, BlockHeader, CompactCiphertext, Fee, OrchardAction, OrchardData, OutPoint,
    SaplingData, SaplingOutput, SaplingSpend, Script, Transaction, TransparentData,
    TransparentOutput, TreeSize, TreeSizes, Zatoshis,
};

use crate::HASH;

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

/// Block at `height` carrying every pool (dropped pool = missing field), its fees and its tree
/// sizes
///
/// - hash = `[height as u8; 32]` (predictable by-hash lookup); sizes = 10 / 20 / 30
/// - its one tx priced at 5 000 zat (value-balance's job to derive, stated here)
pub fn block(height: u32) -> (Block, BlockFees, TreeSizes) {
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
    let block = Block::new(
        BlockHeader::for_tests(
            height,
            bytes32(height as u8),
            bytes32(height.wrapping_sub(1) as u8),
            1_700_000_000 + height,
        ),
        vec![tx],
    );
    let fees = BlockFees {
        height: block.header().height,
        hash: block.header().hash,
        fees: vec![Fee::Paid(Zatoshis::new(5_000).expect("in range"))],
    };
    let sizes = TreeSizes {
        sapling: TreeSize::from(10),
        orchard: TreeSize::from(20),
        ironwood: TreeSize::from(30),
    };
    (block, fees, sizes)
}
