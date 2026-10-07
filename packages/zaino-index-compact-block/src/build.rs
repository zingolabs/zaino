//! Domain [`Block`] + derived [`TreeSizes`] + [`BlockFees`] → the wire bytes the store holds
//!
//! - encoded once at index time (serving never decodes a record)
//! - lives here, not on the domain types (`zaino-primitives` !→ `zaino-proto`)

use bytes::Bytes;
use prost::Message;
use zaino_primitives::types::{
    Block, BlockFees, CompactCiphertext, Fee, OrchardAction, Transaction, TreeSizes, Zatoshis,
};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_proto::proto::compact_formats as cf;

use crate::HASH;

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
            .zip(&fees.fees)
            .zip(0..)
            .map(|((tx, fee), index)| compact_tx(index, tx, paid(fee)))
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
/// - `fee` = `None` (coinbase, unpriced mempool tx) or >= 2^32 → 0 (zaino policy: `uint32`
///   "present if the server can provide it", no presence bit)
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
    use std::slice;

    use zaino_primitives::types::TreeSize;
    use zaino_proto::frame::framed_len;

    use super::*;
    use crate::{
        project::{record_hash, record_sizes},
        testing::block,
    };

    #[test]
    fn a_record_decodes_back_to_every_pool_it_was_built_from() {
        let (block, fees) = block(1);
        let sizes = TreeSizes {
            sapling: TreeSize::from(10),
            orchard: TreeSize::from(20),
            ironwood: TreeSize::from(30),
        };
        let framed = encode_compact_block(&block, &fees, &sizes);
        assert_eq!(framed_len(&framed), Some(framed.len()), "gRPC length prefix");
        let decoded = cf::CompactBlock::decode(&framed[FRAME_HEADER..]).expect("decodes as proto");

        let hash = <[u8; HASH]>::from(block.header().hash);
        let prev = <[u8; HASH]>::from(block.header().prev_hash);
        assert_eq!((decoded.height, decoded.time), (1, block.header().time));
        assert_eq!((decoded.hash, decoded.prev_hash), (hash.to_vec(), prev.to_vec()));
        assert_eq!(record_hash(&framed), Some(hash), "framing walk reads the proto's hash");
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
        assert_eq!(record_sizes(&framed), Some(sizes), "framing walk reads chainMetadata back");
        let cut = &framed[..framed.len() - 1];
        assert_eq!(record_sizes(cut), None, "cut inside chainMetadata");

        assert_eq!(tx.fee, 5_000, "fee from the block's BlockFees");
    }

    /// Wire `fee` = `uint32`, no presence: unknown (coinbase, unpriced mempool tx) and past
    /// `u32::MAX` both write 0 ("not provided"), never a saturated lie
    #[test]
    fn fee_is_exact_within_u32_and_unset_otherwise() {
        let (block, _) = block(1);
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

    /// Fees paired by hash: another block's (a reorg's stale item) = a bug, not a fee
    #[test]
    #[should_panic(expected = "encoded with another block's fees")]
    fn encoding_with_another_blocks_fees_panics() {
        let (block, _) = block(1);
        let (_, stranger) = crate::testing::block(2);
        let _ = encode_compact_block(&block, &stranger, &TreeSizes::ZERO);
    }

    /// Decoded projection = exactly the requested pools, nothing else changed (the walk rewrites
    /// framing: an off-by-one corrupts an unrelated field)
    #[test]
    fn projection_drops_only_the_pools_not_requested() {
        use crate::{project::project, Pools};

        let (block, fees) = block(0);
        let stored = encode_compact_block(&block, &fees, &TreeSizes::ZERO);
        let full = cf::CompactBlock::decode(&stored[FRAME_HEADER..]).expect("decode");

        // every pool requested: the stored bytes, untouched
        assert_eq!(
            project(slice::from_ref(&stored), Pools::ALL).expect("all pools"),
            stored,
            "identity"
        );

        // default (shielded only): transparent gone, shielded intact
        let shielded = project(slice::from_ref(&stored), Pools::default()).expect("default");
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
        assert_eq!(framed_len(&shielded), Some(shielded.len()));
        assert!(shielded.len() < stored.len(), "dropping fields shrinks it");

        // one pool at a time: each selection keeps exactly its own
        let orchard = Pools { sapling: false, orchard: true, ironwood: false, transparent: false };
        let orchard_only = project(slice::from_ref(&stored), orchard).expect("orchard only");
        let decoded = cf::CompactBlock::decode(&orchard_only[FRAME_HEADER..]).expect("decodes");
        let tx = &decoded.vtx[0];

        assert_eq!(tx.actions, full.vtx[0].actions, "orchard kept");
        assert!(tx.spends.is_empty(), "sapling spends dropped");
        assert!(tx.outputs.is_empty(), "sapling outputs dropped");
        assert!(tx.ironwood_actions.is_empty(), "ironwood dropped");
        assert!(tx.vin.is_empty());
        assert_eq!(tx.txid, full.vtx[0].txid, "identity survives every selection");
    }

    /// lightwalletd `FilterTxPool`: a tx left with no component after projection is dropped, for
    /// every pool selection (`ALL` included), from a block and one at a time from the mempool; the
    /// block stays, even with no tx left; the stored record (`GetBlock`) keeps every tx
    #[test]
    fn projection_drops_transactions_left_with_no_component() {
        use zaino_primitives::testing::Chain;
        use zaino_primitives::types::{
            OrchardData, OutPoint, SaplingData, SaplingOutput, Script, TransparentData,
            TransparentOutput,
        };

        use crate::{
            project::{project, project_tx_at},
            Pools,
        };

        let out = |value| TransparentOutput {
            value: Zatoshis::new(value).expect("in range"),
            script: Script::new(vec![0x76, 0xa9, 0x14]),
        };
        let action = OrchardAction {
            nullifier: [0x77; HASH].into(),
            cmx: [0x78; HASH].into(),
            ephemeral_key: [0x79; HASH].into(),
            enc_ciphertext: [0x7a; CompactCiphertext::LENGTH].into(),
        };
        let tx = |seed: u8| Transaction {
            txid: [seed; HASH].into(),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        };
        let coinbase = Transaction {
            transparent: TransparentData {
                coinbase: true,
                inputs: vec![],
                outputs: vec![out(625)],
            },
            ..tx(0)
        };
        let componentless = tx(1);
        let sapling_only = Transaction {
            sapling: SaplingData {
                outputs: vec![SaplingOutput {
                    cmu: [0x44; HASH].into(),
                    ephemeral_key: [0x55; HASH].into(),
                    enc_ciphertext: [0x66; CompactCiphertext::LENGTH].into(),
                }],
                ..Default::default()
            },
            ..tx(2)
        };
        let orchard_only = Transaction {
            orchard: OrchardData { actions: vec![action], ..Default::default() },
            ..tx(3)
        };
        let transparent_only = Transaction {
            transparent: TransparentData {
                coinbase: false,
                inputs: vec![OutPoint { txid: [0x22; HASH].into(), vout: 1 }],
                outputs: vec![out(100)],
            },
            ..tx(4)
        };
        let mut chain = Chain::new();
        let eight = chain.extend(chain.genesis().hash, 8);
        let txs =
            vec![coinbase.clone(), componentless, sapling_only, orchard_only, transparent_only];
        let nine = chain.mine_with(eight.hash, txs);
        let ten = chain.mine_with(nine.hash, vec![coinbase]);
        let block = chain.block(nine.hash);
        let paid = Fee::Paid(Zatoshis::new(1_000).expect("in range"));
        let fees = BlockFees {
            height: block.header().height,
            hash: block.header().hash,
            fees: vec![Fee::Coinbase, paid, paid, paid, paid],
        };
        let sizes = TreeSizes {
            sapling: TreeSize::from(1),
            orchard: TreeSize::from(1),
            ironwood: TreeSize::from(0),
        };
        let stored = encode_compact_block(block, &fees, &sizes);
        let indices = |pools| {
            let projected = project(slice::from_ref(&stored), pools).expect("walks");
            let decoded = cf::CompactBlock::decode(&projected[FRAME_HEADER..]).expect("decodes");
            decoded.vtx.iter().map(|tx| tx.index).collect::<Vec<_>>()
        };
        let transparent =
            Pools { sapling: false, orchard: false, ironwood: false, transparent: true };
        let orchard = Pools { sapling: false, orchard: true, ironwood: false, transparent: false };

        let full = cf::CompactBlock::decode(&stored[FRAME_HEADER..]).expect("decodes");
        assert_eq!(full.vtx.len(), 5, "stored record (GetBlock) keeps every tx");
        assert_eq!(indices(Pools::default()), [2, 3], "shielded: coinbase + transparent-only gone");
        assert_eq!(indices(Pools::ALL), [0, 2, 3, 4], "all: only the component-less tx gone");
        assert_eq!(indices(transparent), [0, 4]);
        assert_eq!(indices(orchard), [3]);

        // the same transactions one at a time from the mempool: the same rule, each re-slotted
        let mempool = |pools| {
            let fee = Some(Zatoshis::new(1_000).expect("in range"));
            let kept = block.transactions().iter().enumerate().filter_map(|(slot, tx)| {
                let rendered = compact_tx(0, tx, fee);
                let framed = project_tx_at(&rendered.encode_to_vec(), slot as u64, pools)?;
                let decoded = cf::CompactTx::decode(&framed[FRAME_HEADER..]).expect("decodes");
                assert_eq!(
                    (&decoded.txid, decoded.fee),
                    (&rendered.txid, 1_000),
                    "slot {slot}: identity + fee"
                );
                Some(decoded.index)
            });
            kept.collect::<Vec<_>>()
        };
        assert_eq!(mempool(Pools::default()), [2, 3], "shielded: as from a block");
        assert_eq!(mempool(Pools::ALL), [0, 2, 3, 4]);
        assert_eq!(mempool(transparent), [0, 4]);
        assert_eq!(mempool(orchard), [3]);

        let coinbase_only = chain.block(ten.hash);
        let fees = BlockFees {
            height: coinbase_only.header().height,
            hash: coinbase_only.header().hash,
            fees: vec![Fee::Coinbase],
        };
        let stored = encode_compact_block(coinbase_only, &fees, &sizes);
        let shielded = project(slice::from_ref(&stored), Pools::default()).expect("walks");
        let decoded = cf::CompactBlock::decode(&shielded[FRAME_HEADER..]).expect("decodes");
        let full = cf::CompactBlock::decode(&stored[FRAME_HEADER..]).expect("decodes");
        assert_eq!(decoded, cf::CompactBlock { vtx: vec![], ..full }, "block kept, no tx left");
    }
}
