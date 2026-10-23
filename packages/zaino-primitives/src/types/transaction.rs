use zcash_protocol::value::{ZatBalance, Zatoshis};
use zcash_protocol::{ShieldedPool, TxId};
use zcash_transparent::address::Script;
use zcash_transparent::bundle::OutPoint;

use super::{CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier};

/// - `txid`: pre-v5 = SHA-256d of the bytes, v5+ = ZIP-244 id
/// - No position (slot = [`Block::transactions`](super::Block::transactions) order, mempool: none)
/// - `ironwood` = Orchard-shaped, own field (own tree, value balance, compact-block filter)
#[derive(Debug, Clone)]
pub struct Transaction {
    pub txid: TxId,
    pub transparent: TransparentData,
    pub sprout: SproutData,
    pub sapling: SaplingData,
    pub orchard: OrchardData,
    pub ironwood: OrchardData,
}

impl Transaction {
    /// Heap bytes held (allocation capacities, inline struct excluded)
    pub(crate) fn heap_size(&self) -> usize {
        fn slots<T>(items: &Vec<T>) -> usize {
            size_of::<T>() * items.capacity()
        }
        let transparent = &self.transparent;
        let scripts: usize =
            transparent.outputs.iter().map(|output| output.script.0 .0.capacity()).sum();
        slots(&transparent.inputs)
            + slots(&transparent.outputs)
            + scripts
            + slots(&self.sapling.spends)
            + slots(&self.sapling.outputs)
            + slots(&self.orchard.actions)
            + slots(&self.ironwood.actions)
    }

    /// Nullifiers revealed in `pool`, in order (sapling spends, orchard / ironwood actions)
    pub fn nullifiers(&self, pool: ShieldedPool) -> impl Iterator<Item = Nullifier> + '_ {
        let (spends, actions) = match pool {
            ShieldedPool::Sapling => (&self.sapling.spends[..], &[][..]),
            ShieldedPool::Orchard => (&[][..], &self.orchard.actions[..]),
            ShieldedPool::Ironwood => (&[][..], &self.ironwood.actions[..]),
        };
        let actions = actions.iter().map(|action| action.nullifier);
        spends.iter().map(|spend| spend.nullifier).chain(actions)
    }

    /// Leaves appended to `pool`'s tree, in order (sapling `cmu`s, orchard / ironwood `cmx`s)
    pub fn note_commitments(
        &self,
        pool: ShieldedPool,
    ) -> impl Iterator<Item = NoteCommitment> + '_ {
        let (outputs, actions) = match pool {
            ShieldedPool::Sapling => (&self.sapling.outputs[..], &[][..]),
            ShieldedPool::Orchard => (&[][..], &self.orchard.actions[..]),
            ShieldedPool::Ironwood => (&[][..], &self.ironwood.actions[..]),
        };
        let actions = actions.iter().map(|action| action.cmx);
        outputs.iter().map(|output| output.cmu).chain(actions)
    }

    pub fn value_balance(&self, pool: ShieldedPool) -> ZatBalance {
        match pool {
            ShieldedPool::Sapling => self.sapling.value_balance,
            ShieldedPool::Orchard => self.orchard.value_balance,
            ShieldedPool::Ironwood => self.ironwood.value_balance,
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentOutput {
    pub value: Zatoshis,
    pub script: Script,
}

/// Value only (no compact-format fields)
///
/// - `value_balance` = Σ `vpub_new` − Σ `vpub_old`, v2–v4 (+ = into the transparent value pool,
///   protocol.pdf §4.12)
#[derive(Debug, Clone, Default)]
pub struct SproutData {
    pub value_balance: ZatBalance,
}

/// `value_balance` = `valueBalanceSapling` (+ = into the transparent value pool, spec §4.13)
#[derive(Debug, Clone, Default)]
pub struct SaplingData {
    pub spends: Vec<SaplingSpend>,
    pub outputs: Vec<SaplingOutput>,
    pub value_balance: ZatBalance,
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
///   value pool, protocol.pdf §4.14, ZIP-229)
#[derive(Debug, Clone, Default)]
pub struct OrchardData {
    pub actions: Vec<OrchardAction>,
    pub value_balance: ZatBalance,
}

/// `enc_ciphertext` = compact head (52 bytes, enough to scan)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchardAction {
    pub nullifier: Nullifier,
    pub cmx: NoteCommitment,
    pub ephemeral_key: EphemeralKey,
    pub enc_ciphertext: CompactCiphertext,
}
