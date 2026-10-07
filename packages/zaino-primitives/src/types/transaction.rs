//! Transaction and per-pool data.

use super::{
    CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier, OutputIndex, Script,
    SignedZatoshis, TransactionId, Zatoshis,
};

/// A transaction within a block.
///
/// - No position field (its slot = the order of [`Block::transactions`](super::Block::transactions);
///   a mempool transaction has none)
#[derive(Debug, Clone)]
pub struct Transaction {
    /// Transaction id.
    ///
    /// NOTE: Transaction hash vs transaction ID
    /// - In pre V5 transactions this is the transaction hash (sha256 of serialized tx).
    /// - From V5 onwards this field is the transaction ID (as defined in [zip 224](https://github.com/zcash/zips/blob/main/zips/zip-0244.rst).
    pub txid: TransactionId,
    /// Transparent pool data.
    pub transparent: TransparentData,
    pub sprout: SproutData,
    /// Sapling pool data.
    pub sapling: SaplingData,
    /// Orchard pool data.
    pub orchard: OrchardData,
    /// Ironwood pool data (NU6.3).
    ///
    /// Ironwood actions are structurally identical to Orchard actions, so the
    /// pool reuses [`OrchardData`] rather than duplicating the shape. It is a
    /// separate field, not merged into `orchard`: the two pools have separate
    /// commitment trees, separate value balances, and are independently
    /// selectable by the compact-block pool filter.
    pub ironwood: OrchardData,
}

impl Transaction {
    /// Heap bytes held (allocation capacities; the inline struct excluded)
    pub(crate) fn heap_size(&self) -> usize {
        fn slots<T>(items: &Vec<T>) -> usize {
            size_of::<T>() * items.capacity()
        }
        let transparent = &self.transparent;
        let scripts: usize =
            transparent.outputs.iter().map(|output| output.script.heap_size()).sum();
        slots(&transparent.inputs)
            + slots(&transparent.outputs)
            + scripts
            + slots(&self.sapling.spends)
            + slots(&self.sapling.outputs)
            + slots(&self.orchard.actions)
            + slots(&self.ironwood.actions)
    }
}

/// Transparent pool data within a transaction.
#[derive(Debug, Clone, Default)]
pub struct TransparentData {
    /// Its one input is the coinbase input (null prevout; protocol.pdf#coinbasetransactions §3.11):
    /// elided from `inputs`, it spends nothing
    pub coinbase: bool,
    /// Transparent inputs, each = the outpoint it spends
    pub inputs: Vec<OutPoint>,
    /// Transparent outputs.
    pub outputs: Vec<TransparentOutput>,
}

/// One transparent output, named by its transaction and position
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutPoint {
    pub txid: TransactionId,
    pub vout: OutputIndex,
}

impl OutPoint {
    pub const LEN: usize = 32 + 4;

    /// `txid ‖ vout` big-endian: byte order = `Ord` (a txid leads: uniform, shardable bytes)
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[..32].copy_from_slice(&<[u8; 32]>::from(self.txid));
        out[32..].copy_from_slice(&self.vout.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8; Self::LEN]) -> Self {
        let (txid, vout) = bytes.split_at(32);
        Self {
            txid: TransactionId::from(<[u8; 32]>::try_from(txid).expect("32 bytes")),
            vout: u32::from_be_bytes(vout.try_into().expect("4 bytes")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden layout: txid bytes as held, then `vout` big-endian (so byte order = key order)
    #[test]
    fn an_outpoint_is_its_golden_bytes_and_orders_like_them() {
        let outpoint = OutPoint { txid: TransactionId::from([0xab; 32]), vout: 0x0102_0304 };
        let golden = [&[0xab; 32][..], &[0x01, 0x02, 0x03, 0x04]].concat();
        assert_eq!(outpoint.encode().as_slice(), golden);
        assert_eq!(OutPoint::decode(&outpoint.encode()), outpoint);

        let later = OutPoint { vout: 0x0102_0305, ..outpoint };
        assert!(outpoint < later && outpoint.encode() < later.encode(), "derived Ord = byte order");
    }
}

/// A transparent output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentOutput {
    /// Value in zatoshis.
    pub value: Zatoshis,
    /// Output script.
    pub script: Script,
}

/// Sprout pool data within a transaction (value only: no compact-format fields)
#[derive(Debug, Clone, Default)]
pub struct SproutData {
    /// Σ `vpub_new` − Σ `vpub_old` over the JoinSplits, v2–v4 only (+ = into the transparent
    /// transaction value pool; protocol.pdf#joinsplitbalance §4.12)
    pub value_balance: SignedZatoshis,
}

/// Sapling pool data within a transaction.
#[derive(Debug, Clone, Default)]
pub struct SaplingData {
    /// Sapling spends (nullifiers).
    pub spends: Vec<SaplingSpend>,
    /// Sapling outputs.
    pub outputs: Vec<SaplingOutput>,
    /// `valueBalanceSapling` (+ = into the transparent transaction value pool;
    /// protocol.pdf#saplingbalance §4.13)
    pub value_balance: SignedZatoshis,
}

/// A Sapling spend: the nullifier that marks a note as consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingSpend {
    /// Nullifier.
    pub nullifier: Nullifier,
}

/// A Sapling output: commitment + detection material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingOutput {
    /// Note commitment (cmu).
    pub cmu: NoteCommitment,
    /// Ephemeral key for recipient detection.
    pub ephemeral_key: EphemeralKey,
    /// Compact ciphertext head (52 bytes, enough for scanning).
    pub enc_ciphertext: CompactCiphertext,
}

/// Orchard pool data within a transaction.
#[derive(Debug, Clone, Default)]
pub struct OrchardData {
    /// Orchard actions (each is both a spend and an output).
    pub actions: Vec<OrchardAction>,
    /// `valueBalanceOrchard` / `valueBalanceIronwood` (+ = into the transparent transaction value
    /// pool; protocol.pdf#orchardbalance §4.14, zip-0229)
    pub value_balance: SignedZatoshis,
}

/// An Orchard action: nullifier + commitment + detection material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchardAction {
    /// Nullifier.
    pub nullifier: Nullifier,
    /// Note commitment (cmx).
    pub cmx: NoteCommitment,
    /// Ephemeral key for recipient detection.
    pub ephemeral_key: EphemeralKey,
    /// Compact ciphertext head (52 bytes, enough for scanning).
    pub enc_ciphertext: CompactCiphertext,
}
