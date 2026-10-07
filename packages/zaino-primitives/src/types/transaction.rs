//! Transaction and per-pool data

use super::{
    CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier, OutputIndex, Script,
    SignedZatoshis, TransactionId, Zatoshis,
};

/// - `txid`: pre-v5 = SHA-256d of the bytes, v5+ = ZIP-244 id
/// - No position (slot = [`Block::transactions`](super::Block::transactions) order; mempool: none)
/// - `ironwood` = Orchard-shaped, own field (own tree, value balance, compact-block filter)
#[derive(Debug, Clone)]
pub struct Transaction {
    pub txid: TransactionId,
    pub transparent: TransparentData,
    pub sprout: SproutData,
    pub sapling: SaplingData,
    pub orchard: OrchardData,
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

/// - `coinbase`: its one input (null prevout, protocol.pdf §3.11) elided from `inputs`
/// - `inputs`: each = the outpoint it spends
#[derive(Debug, Clone, Default)]
pub struct TransparentData {
    pub coinbase: bool,
    pub inputs: Vec<OutPoint>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentOutput {
    pub value: Zatoshis,
    pub script: Script,
}

/// Value only (no compact-format fields)
///
/// - `value_balance` = Σ `vpub_new` − Σ `vpub_old`, v2–v4 (+ = into the transparent value pool;
///   protocol.pdf §4.12)
#[derive(Debug, Clone, Default)]
pub struct SproutData {
    pub value_balance: SignedZatoshis,
}

/// `value_balance` = `valueBalanceSapling` (+ = into the transparent value pool; spec §4.13)
#[derive(Debug, Clone, Default)]
pub struct SaplingData {
    pub spends: Vec<SaplingSpend>,
    pub outputs: Vec<SaplingOutput>,
    pub value_balance: SignedZatoshis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingSpend {
    pub nullifier: Nullifier,
}

/// `enc_ciphertext` = compact head (52 bytes, enough to scan)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaplingOutput {
    pub cmu: NoteCommitment,
    pub ephemeral_key: EphemeralKey,
    pub enc_ciphertext: CompactCiphertext,
}

/// - Action = spend + output
/// - `value_balance` = `valueBalanceOrchard` / `valueBalanceIronwood` (+ = into the transparent
///   value pool; protocol.pdf §4.14, ZIP-229)
#[derive(Debug, Clone, Default)]
pub struct OrchardData {
    pub actions: Vec<OrchardAction>,
    pub value_balance: SignedZatoshis,
}

/// `enc_ciphertext` = compact head (52 bytes, enough to scan)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchardAction {
    pub nullifier: Nullifier,
    pub cmx: NoteCommitment,
    pub ephemeral_key: EphemeralKey,
    pub enc_ciphertext: CompactCiphertext,
}
