//! Domain -> wire conversion, owned by the adapter.
//!
//! Conversion lives here, not on the domain types, so the domain crate never
//! depends on `zaino-proto`. A local extension trait keeps the `to_wire()`
//! naming and direction while respecting the orphan rule (foreign domain type,
//! local trait).

use zaino_core::{
    BlockId, ChainMetadata, CompactBlock, CompactCiphertext, Nullifier, OrchardAction,
    PreIndexCompactTx, SaplingOutput, TransparentInput, TransparentOutput,
};
use zaino_proto::proto::compact_formats as cf;
use zaino_proto::proto::service as proto;

pub(crate) trait ToWire {
    type Wire;
    fn to_wire(self) -> Self::Wire;
}

impl ToWire for BlockId {
    type Wire = proto::BlockId;

    fn to_wire(self) -> proto::BlockId {
        proto::BlockId {
            height: u64::from(self.height),
            hash: <[u8; 32]>::from(self.hash).to_vec(),
        }
    }
}

impl ToWire for CompactBlock {
    type Wire = cf::CompactBlock;

    fn to_wire(self) -> cf::CompactBlock {
        cf::CompactBlock {
            height: u64::from(self.height),
            hash: <[u8; 32]>::from(self.hash).to_vec(),
            prev_hash: <[u8; 32]>::from(self.prev_hash).to_vec(),
            // `BlockTime` is a Unix-epoch `u32`; the wire field is the same.
            time: self.time,
            // The full 80-byte header is optional in the compact format and the
            // index does not retain it, so it rides out empty.
            header: Vec::new(),
            vtx: self
                .transactions
                .into_iter()
                .enumerate()
                .map(|(index, tx)| compact_tx_to_wire(index_as_u64(index), tx))
                .collect(),
            chain_metadata: Some(self.chain_metadata.to_wire()),
        }
    }
}

impl ToWire for ChainMetadata {
    type Wire = cf::ChainMetadata;

    fn to_wire(self) -> cf::ChainMetadata {
        cf::ChainMetadata {
            sapling_commitment_tree_size: u32::from(self.sapling_tree_size),
            orchard_commitment_tree_size: u32::from(self.orchard_tree_size),
            ironwood_commitment_tree_size: u32::from(self.ironwood_tree_size),
        }
    }
}

/// A transaction's position within its block. `usize -> u64` is lossless on
/// every supported platform (a block holds at most `usize` transactions).
fn index_as_u64(index: usize) -> u64 {
    u64::try_from(index).expect("a block's tx count fits u64")
}

/// One compact transaction. `index` is its position within the block. The
/// transparent, shielded, and ironwood components each map to their wire shape;
/// `fee` is left unset (0) — a stateless index cannot compute it.
fn compact_tx_to_wire(index: u64, tx: PreIndexCompactTx) -> cf::CompactTx {
    cf::CompactTx {
        index,
        txid: <[u8; 32]>::from(tx.txid).to_vec(),
        fee: 0,
        spends: tx.sapling_nullifiers.into_iter().map(spend).collect(),
        outputs: tx.sapling_outputs.into_iter().map(sapling_output).collect(),
        actions: tx.orchard_actions.into_iter().map(orchard_action).collect(),
        ironwood_actions: tx
            .ironwood_actions
            .into_iter()
            .map(orchard_action)
            .collect(),
        vin: tx.transparent_inputs.into_iter().map(txin).collect(),
        vout: tx.transparent_outputs.into_iter().map(txout).collect(),
    }
}

fn spend(nullifier: Nullifier) -> cf::CompactSaplingSpend {
    cf::CompactSaplingSpend {
        nf: <[u8; 32]>::from(nullifier).to_vec(),
    }
}

fn sapling_output(output: SaplingOutput) -> cf::CompactSaplingOutput {
    cf::CompactSaplingOutput {
        cmu: <[u8; 32]>::from(output.cmu).to_vec(),
        ephemeral_key: <[u8; 32]>::from(output.ephemeral_key).to_vec(),
        ciphertext: <[u8; CompactCiphertext::LENGTH]>::from(output.enc_ciphertext).to_vec(),
    }
}

fn orchard_action(action: OrchardAction) -> cf::CompactOrchardAction {
    cf::CompactOrchardAction {
        nullifier: <[u8; 32]>::from(action.nullifier).to_vec(),
        cmx: <[u8; 32]>::from(action.cmx).to_vec(),
        ephemeral_key: <[u8; 32]>::from(action.ephemeral_key).to_vec(),
        ciphertext: <[u8; CompactCiphertext::LENGTH]>::from(action.enc_ciphertext).to_vec(),
    }
}

fn txin(input: TransparentInput) -> cf::CompactTxIn {
    cf::CompactTxIn {
        prevout_txid: <[u8; 32]>::from(input.prev_txid).to_vec(),
        prevout_index: input.prev_index,
    }
}

fn txout(output: TransparentOutput) -> cf::TxOut {
    cf::TxOut {
        value: u64::from(output.value),
        script_pub_key: Vec::<u8>::from(output.script),
    }
}

/// Lowercase hex, so a domain id can ride out on a wire string field without a
/// hex dependency.
pub(crate) fn to_hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
