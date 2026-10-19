//! Domain → protobuf encoders: [`encode_compact_block`] (stored record), [`compact_tx`]
//!
//! - encoded once at index time (serving never decodes a record)
//! - [`compact_tx`] = also the mempool encoder (`zaino-grpc` `routes/mempool.rs`)
//! - lives here, not on the domain types (`zaino-primitives` !→ `zaino-proto`)

use bytes::Bytes;
use prost::Message;
use zaino_primitives::types::{Block, BlockFees, Fee, OrchardAction, Transaction, TreeSizes};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_proto::proto::compact_formats;
use zcash_protocol::value::Zatoshis;

/// `block` → gRPC-framed `CompactBlock` bytes, every pool included (`project` prunes on read)
///
/// - `fees` = `block`'s own (asserted), one per tx: each `CompactTx.fee`
/// - `sizes` = cumulative tree sizes after `block` ([`fold`](crate::fold) derives)
pub fn encode_compact_block(block: &Block, fees: &BlockFees, sizes: &TreeSizes) -> Bytes {
    let header = &block.header();
    let height = header.height;
    assert!(fees.belongs_to(block), "{height} encoded with another block's fees");
    let paid = |fee: &Fee| match fee {
        Fee::Coinbase => None,
        Fee::Paid(fee) => Some(*fee),
    };
    let compact = compact_formats::CompactBlock {
        height: height.into(),
        hash: header.hash.0.to_vec(),
        prev_hash: header.prev_hash.0.to_vec(),
        time: header.time,
        // always unset (compact_formats.proto `CompactBlock`: header data only via future methods)
        header: Vec::new(),
        vtx: block
            .transactions()
            .iter()
            .zip(&fees.fees)
            .zip(0..)
            .map(|((tx, fee), index)| compact_tx(index, tx, paid(fee)))
            .collect(),
        chain_metadata: Some(compact_formats::ChainMetadata {
            sapling_commitment_tree_size: u32::from(sizes.sapling),
            orchard_commitment_tree_size: u32::from(sizes.orchard),
            ironwood_commitment_tree_size: u32::from(sizes.ironwood),
        }),
    };

    let mut framed = Vec::with_capacity(FRAME_HEADER + compact.encoded_len());
    frame_into(&mut framed, |out| compact.encode_raw(out));
    Bytes::from(framed)
}

/// `tx` → `CompactTx`
///
/// - `index` = position in block (mempool: stream slot)
/// - `fee` = `None` (coinbase, unpriced mempool tx) or >= 2^32 → 0 (zaino policy: `uint32`
///   "present if the server can provide it", no presence bit)
pub fn compact_tx(
    index: u64,
    tx: &Transaction,
    fee: Option<Zatoshis>,
) -> compact_formats::CompactTx {
    // Orchard + Ironwood share one action shape
    let action = |action: &OrchardAction| compact_formats::CompactOrchardAction {
        nullifier: action.nullifier.0.to_vec(),
        cmx: action.cmx.0.to_vec(),
        ephemeral_key: action.ephemeral_key.0.to_vec(),
        ciphertext: action.enc_ciphertext.0.to_vec(),
    };

    compact_formats::CompactTx {
        index,
        txid: tx.txid.as_ref().to_vec(),
        fee: fee.and_then(|fee| u32::try_from(fee.into_u64()).ok()).unwrap_or(0),
        spends: tx
            .sapling
            .spends
            .iter()
            .map(|spend| compact_formats::CompactSaplingSpend { nf: spend.nullifier.0.to_vec() })
            .collect(),
        outputs: tx
            .sapling
            .outputs
            .iter()
            .map(|output| compact_formats::CompactSaplingOutput {
                cmu: output.cmu.0.to_vec(),
                ephemeral_key: output.ephemeral_key.0.to_vec(),
                ciphertext: output.enc_ciphertext.0.to_vec(),
            })
            .collect(),
        actions: tx.orchard.actions.iter().map(action).collect(),
        ironwood_actions: tx.ironwood.actions.iter().map(action).collect(),
        vin: tx
            .transparent
            .inputs
            .iter()
            .map(|input| compact_formats::CompactTxIn {
                prevout_txid: input.hash().to_vec(),
                prevout_index: input.n(),
            })
            .collect(),
        vout: tx
            .transparent
            .outputs
            .iter()
            .map(|output| compact_formats::TxOut {
                value: output.value.into_u64(),
                script_pub_key: output.script.0 .0.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::testing::{p2pkh, MockChain};
    use zaino_primitives::types::{CompactCiphertext, EphemeralKey, TreeSize};
    use zaino_proto::frame::framed_len;
    use zcash_transparent::bundle::OutPoint;

    use super::*;
    use crate::HASH;

    /// Block 2's tx (spends block 1's output 7, every pool, fee 5 000) → one gRPC frame decoding
    /// to every field built from it (transparent kept: pruning = read-side)
    #[test]
    fn a_record_decodes_back_to_every_pool_it_was_built_from() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest();
        chain.mine(|b| {
            b.coinbase(|c| (0..8).fold(c.txid([0x22; 32]), |c, _| c.pay(&alice, 17_345)))
        });
        let two = chain.mine(|b| {
            b.coinbase(|c| c.txid([0x0c; 32])).tx(|t| {
                t.txid([0x11; 32])
                    .spend(OutPoint::new([0x22; 32], 7))
                    .pay(&alice, 12_345)
                    .fee(5_000)
                    .sapling_spend([0x33; 32])
                    .sapling_output(0x44)
                    .orchard_action([0x77; 32], 0x78)
                    .ironwood_action([0x88; 32], 0x89)
                    .ironwood_action([0x99; 32], 0x9a)
            })
        });
        let (block, fees) = (chain.block(two.hash), chain.fees(two.hash));
        let sizes = TreeSizes {
            sapling: TreeSize::from(10),
            orchard: TreeSize::from(20),
            ironwood: TreeSize::from(30),
        };
        let framed = encode_compact_block(block, &fees, &sizes);
        assert_eq!(framed_len(&framed), Some(framed.len()), "gRPC length prefix");
        let decoded = compact_formats::CompactBlock::decode(&framed[FRAME_HEADER..])
            .expect("decodes as proto");

        // detection bytes = the builder's own, opaque here
        let shielded = &block.transactions()[1];
        let key = |key: EphemeralKey| key.0.to_vec();
        let ciphertext = |bytes: CompactCiphertext| bytes.0.to_vec();
        let leaf = |leaf: u8| [[leaf].as_slice(), &[0; HASH - 1]].concat();
        let action =
            |nullifier: u8, cmx: u8, built: &OrchardAction| compact_formats::CompactOrchardAction {
                nullifier: vec![nullifier; HASH],
                cmx: leaf(cmx),
                ephemeral_key: key(built.ephemeral_key),
                ciphertext: ciphertext(built.enc_ciphertext),
            };
        let output = &shielded.sapling.outputs[0];
        let ironwood = &shielded.ironwood.actions;
        let expected = compact_formats::CompactBlock {
            height: 2,
            hash: block.header().hash.0.to_vec(),
            prev_hash: block.header().prev_hash.0.to_vec(),
            time: block.header().time,
            header: Vec::new(),
            vtx: vec![
                compact_formats::CompactTx {
                    index: 0,
                    txid: vec![0x0c; HASH],
                    ..compact_formats::CompactTx::default()
                },
                compact_formats::CompactTx {
                    index: 1,
                    txid: vec![0x11; HASH],
                    fee: 5_000,
                    spends: vec![compact_formats::CompactSaplingSpend { nf: vec![0x33; HASH] }],
                    outputs: vec![compact_formats::CompactSaplingOutput {
                        cmu: leaf(0x44),
                        ephemeral_key: key(output.ephemeral_key),
                        ciphertext: ciphertext(output.enc_ciphertext),
                    }],
                    actions: vec![action(0x77, 0x78, &shielded.orchard.actions[0])],
                    ironwood_actions: vec![
                        action(0x88, 0x89, &ironwood[0]),
                        action(0x99, 0x9a, &ironwood[1]),
                    ],
                    vin: vec![compact_formats::CompactTxIn {
                        prevout_txid: vec![0x22; HASH],
                        prevout_index: 7,
                    }],
                    vout: vec![compact_formats::TxOut {
                        value: 12_345,
                        script_pub_key: alice.0 .0.clone(),
                    }],
                },
            ],
            chain_metadata: Some(compact_formats::ChainMetadata {
                sapling_commitment_tree_size: 10,
                orchard_commitment_tree_size: 20,
                ironwood_commitment_tree_size: 30,
            }),
        };
        assert_eq!(decoded, expected);
    }

    /// Wire `fee` = `uint32`, no presence: unknown (coinbase, unpriced mempool tx) and past
    /// `u32::MAX` both write 0 ("not provided"), never a saturated lie
    #[test]
    fn fee_is_exact_within_u32_and_unset_otherwise() {
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| b.tx(|t| t.sapling_output(1)));
        let tx = &chain.block(one.hash).transactions()[1];
        let max = u64::from(u32::MAX);
        for (fee, wire) in [
            (None, 0),
            (Some(0), 0),
            (Some(10_000), 10_000),
            (Some(max), u32::MAX),
            (Some(max + 1), 0),
        ] {
            let fee = fee.map(|zats| Zatoshis::from_u64(zats).expect("in supply"));
            assert_eq!(compact_tx(0, tx, fee).fee, wire, "{fee:?}");
        }
    }

    /// Fees paired by hash: another block's (a reorg's stale item) = a bug, not a fee
    #[test]
    #[should_panic(expected = "encoded with another block's fees")]
    fn encoding_with_another_blocks_fees_panics() {
        let mut chain = MockChain::regtest();
        let (one, two) = (chain.mine_empty(1), chain.mine_empty(1));
        let stranger = chain.fees(two.hash);
        let _ = encode_compact_block(chain.block(one.hash), &stranger, &TreeSizes::ZERO);
    }
}
