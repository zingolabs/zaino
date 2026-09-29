//! Domain [`Block`] + derived [`TreeSizes`] + [`BlockValueBalances`] → the wire bytes the store
//! holds
//!
//! - encoded once at index time (serving never decodes a record)
//! - lives here, not on the domain types (`zaino-primitives` !→ `zaino-proto`)

use bytes::Bytes;
use prost::Message;
use zaino_primitives::types::{
    Block, BlockValueBalances, CompactCiphertext, OrchardAction, Transaction, TreeSizes, Zatoshis,
};
use zaino_proto::proto::compact_formats as cf;

use crate::record::{frame_into, FRAME_HEADER, HASH};

/// `block` → gRPC-framed `CompactBlock` bytes, every pool included (`project` prunes on read)
///
/// - `balances` = `block`'s own (asserted), one per tx: each `CompactTx.fee`
/// - `sizes` = cumulative tree sizes after `block` (`CompactBlockIndexWriter` derives)
pub fn encode_compact_block(
    block: &Block,
    balances: &BlockValueBalances,
    sizes: &TreeSizes,
) -> Bytes {
    let header = &block.header();
    let height = header.height;
    assert!(balances.belongs_to(block), "{height} encoded with another block's value balances");
    let compact = cf::CompactBlock {
        height: height.into(),
        hash: <[u8; HASH]>::from(header.hash).to_vec(),
        prev_hash: <[u8; HASH]>::from(header.prev_hash).to_vec(),
        time: header.time,
        // optional in the compact format (not retained)
        header: Vec::new(),
        vtx: block
            .transactions()
            .iter()
            .zip(&balances.balances)
            .zip(0..)
            .map(|((tx, balance), index)| compact_tx(index, tx, balance.fee()))
            .collect(),
        chain_metadata: Some(cf::ChainMetadata {
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
/// - Option<`fee`> in zatoshis (protocol specifies that 0 is "not provided" or >= 2^32 zatoshis)
pub fn compact_tx(index: u64, tx: &Transaction, fee: Option<Zatoshis>) -> cf::CompactTx {
    // Orchard + Ironwood share one action shape
    let action = |action: &OrchardAction| cf::CompactOrchardAction {
        nullifier: <[u8; HASH]>::from(action.nullifier).to_vec(),
        cmx: <[u8; HASH]>::from(action.cmx).to_vec(),
        ephemeral_key: <[u8; HASH]>::from(action.ephemeral_key).to_vec(),
        ciphertext: <[u8; CompactCiphertext::LENGTH]>::from(action.enc_ciphertext).to_vec(),
    };

    cf::CompactTx {
        index,
        txid: <[u8; HASH]>::from(tx.txid).to_vec(),
        fee: fee.and_then(|fee| u32::try_from(fee.as_u64()).ok()).unwrap_or(0),
        spends: tx
            .sapling
            .spends
            .iter()
            .map(|spend| cf::CompactSaplingSpend {
                nf: <[u8; HASH]>::from(spend.nullifier).to_vec(),
            })
            .collect(),
        outputs: tx
            .sapling
            .outputs
            .iter()
            .map(|output| cf::CompactSaplingOutput {
                cmu: <[u8; HASH]>::from(output.cmu).to_vec(),
                ephemeral_key: <[u8; HASH]>::from(output.ephemeral_key).to_vec(),
                ciphertext: <[u8; CompactCiphertext::LENGTH]>::from(output.enc_ciphertext).to_vec(),
            })
            .collect(),
        actions: tx.orchard.actions.iter().map(action).collect(),
        ironwood_actions: tx.ironwood.actions.iter().map(action).collect(),
        vin: tx
            .transparent
            .inputs
            .iter()
            .map(|input| cf::CompactTxIn {
                prevout_txid: <[u8; HASH]>::from(input.txid).to_vec(),
                prevout_index: input.vout,
            })
            .collect(),
        vout: tx
            .transparent
            .outputs
            .iter()
            .map(|output| cf::TxOut {
                value: u64::from(output.value),
                script_pub_key: output.script.as_bytes().to_vec(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{project::record_hash, testing::block};

    #[test]
    fn a_record_decodes_back_to_every_pool_it_was_built_from() {
        let (block, balances, sizes) = block(1);
        let framed = encode_compact_block(&block, &balances, &sizes);
        let prefix = u32::from_be_bytes(framed[1..5].try_into().expect("len")) as usize;
        assert_eq!(prefix, framed.len() - FRAME_HEADER, "gRPC length prefix");
        let decoded = cf::CompactBlock::decode(&framed[FRAME_HEADER..]).expect("decodes as proto");

        assert_eq!(decoded.height, 1);
        assert_eq!(decoded.hash, [1; HASH].to_vec());
        assert_eq!(record_hash(&framed), Some([1; HASH]), "framing walk reads the proto's hash");
        assert_eq!(record_hash(&framed[..FRAME_HEADER + 4]), None, "cut before the hash");
        assert_eq!(decoded.vtx.len(), 1);

        let tx = &decoded.vtx[0];
        assert_eq!(tx.index, 0);
        assert_eq!(tx.txid, [0x11; HASH].to_vec());

        // every pool survives the round trip (`ironwood_actions` included)
        assert_eq!(tx.spends.len(), 1, "sapling spends");
        assert_eq!(tx.outputs.len(), 1, "sapling outputs");
        assert_eq!(tx.actions.len(), 1, "orchard actions");
        assert_eq!(tx.ironwood_actions.len(), 2, "ironwood actions");
        assert_eq!(tx.ironwood_actions[0].nullifier, [0x88; HASH].to_vec());
        assert_eq!(tx.ironwood_actions[1].nullifier, [0x99; HASH].to_vec());

        // transparent rides in the record (pruning = a read-side decision)
        assert_eq!(tx.vin.len(), 1, "transparent inputs");
        assert_eq!(tx.vin[0].prevout_index, 7);
        assert_eq!(tx.vout.len(), 1, "transparent outputs");
        assert_eq!(tx.vout[0].value, 12_345);

        // tree sizes for all three pools
        let metadata = decoded.chain_metadata.expect("chain metadata");
        assert_eq!(metadata.sapling_commitment_tree_size, 10);
        assert_eq!(metadata.orchard_commitment_tree_size, 20);
        assert_eq!(metadata.ironwood_commitment_tree_size, 30);

        assert_eq!(tx.fee, 5_000, "fee from the tx's value balance");
    }

    /// Wire `fee` = `uint32`, no presence: unknown (coinbase, unpriced mempool tx) and past
    /// `u32::MAX` both write 0 ("not provided"), never a saturated lie
    #[test]
    fn fee_is_exact_within_u32_and_unset_otherwise() {
        let (block, _, _) = block(1);
        let tx = &block.transactions()[0];
        let max = u64::from(u32::MAX);
        for (fee, wire) in [
            (None, 0),
            (Some(0), 0),
            (Some(10_000), 10_000),
            (Some(max), u32::MAX),
            (Some(max + 1), 0),
        ] {
            let fee = fee.map(|zats| Zatoshis::new(zats).expect("in supply"));
            assert_eq!(compact_tx(0, tx, fee).fee, wire, "{fee:?}");
        }
    }

    /// Balances paired by hash: another block's (a reorg's stale item) = a bug, not a fee
    #[test]
    #[should_panic(expected = "encoded with another block's value balances")]
    fn encoding_with_another_blocks_balances_panics() {
        let (block, _, sizes) = block(1);
        let (_, stranger, _) = crate::testing::block(2);
        let _ = encode_compact_block(&block, &stranger, &sizes);
    }

    /// Decoded projection = exactly the requested pools, nothing else changed (the walk rewrites
    /// framing: an off-by-one corrupts an unrelated field)
    #[test]
    fn projection_drops_only_the_pools_not_requested() {
        use crate::{project::project, Pools};

        let (block, balances, sizes) = block(0);
        let stored = encode_compact_block(&block, &balances, &sizes);
        let full = cf::CompactBlock::decode(&stored[FRAME_HEADER..]).expect("decode");

        // every pool requested: the stored bytes, untouched
        assert_eq!(project(&stored, Pools::ALL).expect("all pools"), stored, "identity");

        // default (shielded only): transparent gone, shielded intact
        let shielded = project(&stored, Pools::default()).expect("default");
        let decoded = cf::CompactBlock::decode(&shielded[FRAME_HEADER..]).expect("decodes");
        let tx = &decoded.vtx[0];

        assert!(tx.vin.is_empty(), "transparent inputs dropped");
        assert!(tx.vout.is_empty(), "transparent outputs dropped");
        assert_eq!(tx.spends, full.vtx[0].spends, "sapling spends kept");
        assert_eq!(tx.outputs, full.vtx[0].outputs, "sapling outputs kept");
        assert_eq!(tx.actions, full.vtx[0].actions, "orchard kept");
        assert_eq!(tx.ironwood_actions, full.vtx[0].ironwood_actions, "ironwood kept");

        // nothing outside the pools moved
        assert_eq!(decoded.height, full.height);
        assert_eq!(decoded.hash, full.hash);
        assert_eq!(decoded.prev_hash, full.prev_hash);
        assert_eq!(decoded.time, full.time);
        assert_eq!(decoded.chain_metadata, full.chain_metadata);
        assert_eq!(tx.index, full.vtx[0].index);
        assert_eq!(tx.txid, full.vtx[0].txid);

        // frame length prefix rewritten to the shortened payload
        let prefix = u32::from_be_bytes(shielded[1..5].try_into().expect("len")) as usize;
        assert_eq!(prefix, shielded.len() - FRAME_HEADER);
        assert!(shielded.len() < stored.len(), "dropping fields shrinks it");

        // one pool at a time: each selection keeps exactly its own
        let orchard = Pools { sapling: false, orchard: true, ironwood: false, transparent: false };
        let orchard_only = project(&stored, orchard).expect("orchard only");
        let decoded = cf::CompactBlock::decode(&orchard_only[FRAME_HEADER..]).expect("decodes");
        let tx = &decoded.vtx[0];

        assert_eq!(tx.actions, full.vtx[0].actions, "orchard kept");
        assert!(tx.spends.is_empty(), "sapling spends dropped");
        assert!(tx.outputs.is_empty(), "sapling outputs dropped");
        assert!(tx.ironwood_actions.is_empty(), "ironwood dropped");
        assert!(tx.vin.is_empty());
        assert_eq!(tx.txid, full.vtx[0].txid, "identity survives every selection");
    }
}
