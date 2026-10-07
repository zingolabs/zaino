//! What a test states about a block or a transaction; [`MockChain`](super::MockChain) fills in the
//! rest (txids, detection material, header) and checks it

use super::sha256d;
use crate::types::{
    CompactCiphertext, EphemeralKey, NoteCommitment, Nullifier, OrchardAction, OrchardData,
    OutPoint, SaplingData, SaplingOutput, SaplingSpend, Script, ShieldedPool, SignedZatoshis,
    SproutData, Transaction, TransactionId, TransparentData, TransparentOutput, Zatoshis,
};

/// One block: coinbase in slot 0, then each `tx` / `raw_tx` in call order
#[derive(Debug, Clone)]
pub struct BlockBuilder {
    pub(super) coinbase: TxBuilder,
    pub(super) txs: Vec<Planned>,
    pub(super) time: Option<u32>,
    pub(super) bits: Option<u32>,
}

/// `Raw` = decoded bytes + the bytes (served by a validator double; a `Built` tx has none)
#[derive(Debug, Clone)]
pub(super) enum Planned {
    Built(TxBuilder),
    Raw(Transaction, Vec<u8>),
}

impl BlockBuilder {
    pub(super) fn new() -> Self {
        Self { coinbase: TxBuilder::new(), txs: Vec::new(), time: None, bits: None }
    }

    /// Default: bare (no outputs, txid from the mint counter)
    pub fn coinbase(self, tx: impl FnOnce(TxBuilder) -> TxBuilder) -> Self {
        Self { coinbase: tx(self.coinbase), ..self }
    }

    pub fn tx(mut self, tx: impl FnOnce(TxBuilder) -> TxBuilder) -> Self {
        self.txs.push(Planned::Built(tx(TxBuilder::new())));
        self
    }

    /// Real bytes + their decode (`zaino_source::testing::decoded`; decoding lives above this crate)
    pub fn raw_tx(mut self, (tx, bytes): (Transaction, Vec<u8>)) -> Self {
        self.txs.push(Planned::Raw(tx, bytes));
        self
    }

    /// Header rule tests (default: parent + target spacing; must pass the median time past)
    pub fn time(self, unix: u32) -> Self {
        Self { time: Some(unix), ..self }
    }

    /// Header rule tests under `Work::Varied` (default: the regtest limit)
    pub fn bits(self, bits: u32) -> Self {
        Self { bits: Some(bits), ..self }
    }

    /// Anything a branch's past could refuse (a bare default coinbase: nothing)
    pub(super) fn reads_history(&self) -> bool {
        let coinbase = &self.coinbase;
        let actions = !coinbase.orchard_actions.is_empty() || !coinbase.ironwood_actions.is_empty();
        !self.txs.is_empty() || coinbase.txid.is_some() || actions
    }
}

/// One transaction; zats as `u64` / `i64` (checked against the supply when mined)
#[derive(Debug, Clone)]
pub struct TxBuilder {
    pub(super) txid: Option<TransactionId>,
    pub(super) spends: Vec<OutPoint>,
    pub(super) pays: Vec<(Script, u64)>,
    pub(super) sapling_spends: Vec<Nullifier>,
    pub(super) sapling_outputs: Vec<u32>,
    pub(super) orchard_actions: Vec<(Nullifier, u32)>,
    pub(super) ironwood_actions: Vec<(Nullifier, u32)>,
    pub(super) sapling_balance: i64,
    pub(super) orchard_balance: i64,
    pub(super) ironwood_balance: i64,
    pub(super) sprout_balance: i64,
    pub(super) fee: Option<u64>,
}

impl TxBuilder {
    fn new() -> Self {
        Self {
            txid: None,
            spends: Vec::new(),
            pays: Vec::new(),
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
            orchard_actions: Vec::new(),
            ironwood_actions: Vec::new(),
            sapling_balance: 0,
            orchard_balance: 0,
            ironwood_balance: 0,
            sprout_balance: 0,
            fee: None,
        }
    }

    /// Default: SHA-256d(mint counter ‖ content)
    pub fn txid(self, txid: [u8; 32]) -> Self {
        Self { txid: Some(TransactionId::from(txid)), ..self }
    }

    pub fn spend(mut self, prevout: OutPoint) -> Self {
        self.spends.push(prevout);
        self
    }

    pub fn pay(mut self, script: &Script, zats: u64) -> Self {
        self.pays.push((script.clone(), zats));
        self
    }

    pub fn sapling_spend(mut self, nullifier: [u8; 32]) -> Self {
        self.sapling_spends.push(Nullifier::from(nullifier));
        self
    }

    /// cmu = `leaf` little-endian (canonical under both moduli)
    pub fn sapling_output(mut self, leaf: u32) -> Self {
        self.sapling_outputs.push(leaf);
        self
    }

    /// cmx = `leaf` little-endian
    pub fn orchard_action(mut self, nullifier: [u8; 32], leaf: u32) -> Self {
        self.orchard_actions.push((Nullifier::from(nullifier), leaf));
        self
    }

    /// cmx = `leaf` little-endian
    pub fn ironwood_action(mut self, nullifier: [u8; 32], leaf: u32) -> Self {
        self.ironwood_actions.push((Nullifier::from(nullifier), leaf));
        self
    }

    /// + = out of `pool` into the transparent value pool
    pub fn value_balance(self, pool: ShieldedPool, zats: i64) -> Self {
        match pool {
            ShieldedPool::Sapling => Self { sapling_balance: zats, ..self },
            ShieldedPool::Orchard => Self { orchard_balance: zats, ..self },
            ShieldedPool::Ironwood => Self { ironwood_balance: zats, ..self },
        }
    }

    /// + = out of Sprout into the transparent value pool
    pub fn sprout_balance(self, zats: i64) -> Self {
        Self { sprout_balance: zats, ..self }
    }

    /// Asserted against conservation when mined (unstated = derived)
    pub fn fee(self, zats: u64) -> Self {
        Self { fee: Some(zats), ..self }
    }

    /// What a default txid commits to beside the mint counter (bare = empty)
    ///
    /// - two chains' blocks with different contents never share a hash
    pub(super) fn content(&self) -> Vec<u8> {
        let action = |(nullifier, leaf): &(Nullifier, u32)| {
            [&<[u8; 32]>::from(*nullifier)[..], &leaf.to_le_bytes()].concat()
        };
        let pay = |(script, zats): &(Script, u64)| {
            let len = u32::try_from(script.as_bytes().len()).expect("a test script under 4 GiB");
            [&zats.to_le_bytes()[..], &len.to_le_bytes(), script.as_bytes()].concat()
        };
        let balances = [
            self.sprout_balance,
            self.sapling_balance,
            self.orchard_balance,
            self.ironwood_balance,
        ];
        let balances: Vec<u8> = match balances == [0; 4] {
            true => Vec::new(),
            false => balances.iter().flat_map(|balance| balance.to_le_bytes()).collect(),
        };
        let sections: [(u8, Vec<u8>); 7] = [
            (b's', self.spends.iter().flat_map(OutPoint::encode).collect()),
            (b'p', self.pays.iter().flat_map(pay).collect()),
            (b'n', self.sapling_spends.iter().flat_map(|nf| <[u8; 32]>::from(*nf)).collect()),
            (b'o', self.sapling_outputs.iter().flat_map(|leaf| leaf.to_le_bytes()).collect()),
            (b'a', self.orchard_actions.iter().flat_map(action).collect()),
            (b'i', self.ironwood_actions.iter().flat_map(action).collect()),
            (b'b', balances),
        ];
        let present = sections.into_iter().filter(|(_, bytes)| !bytes.is_empty());
        present.flat_map(|(tag, bytes)| [vec![tag], bytes].concat()).collect()
    }

    /// Coinbase or not: the chain checks that against what it holds
    pub(super) fn into_transaction(self, txid: TransactionId, coinbase: bool) -> Transaction {
        let zats = |zats: u64| {
            Zatoshis::new(zats).unwrap_or_else(|_| panic!("{txid}: {zats} zats past the supply"))
        };
        let signed = |zats: i64| {
            SignedZatoshis::new(zats)
                .unwrap_or_else(|_| panic!("{txid}: balance {zats} past the supply"))
        };
        let outputs = self.pays.into_iter();
        let outputs =
            outputs.map(|(script, value)| TransparentOutput { value: zats(value), script });
        let spends = self.sapling_spends.into_iter().map(|nullifier| SaplingSpend { nullifier });
        let sapling_outputs = (0..).zip(self.sapling_outputs).map(|(index, leaf)| {
            let (ephemeral_key, enc_ciphertext) = detection(txid, ShieldedPool::Sapling, index);
            SaplingOutput { cmu: commitment(leaf), ephemeral_key, enc_ciphertext }
        });
        Transaction {
            txid,
            transparent: TransparentData {
                coinbase,
                inputs: self.spends,
                outputs: outputs.collect(),
            },
            sprout: SproutData { value_balance: signed(self.sprout_balance) },
            sapling: SaplingData {
                spends: spends.collect(),
                outputs: sapling_outputs.collect(),
                value_balance: signed(self.sapling_balance),
            },
            orchard: OrchardData {
                actions: actions(txid, ShieldedPool::Orchard, self.orchard_actions),
                value_balance: signed(self.orchard_balance),
            },
            ironwood: OrchardData {
                actions: actions(txid, ShieldedPool::Ironwood, self.ironwood_actions),
                value_balance: signed(self.ironwood_balance),
            },
        }
    }
}

fn actions(
    txid: TransactionId,
    pool: ShieldedPool,
    of: Vec<(Nullifier, u32)>,
) -> Vec<OrchardAction> {
    let actions = (0..).zip(of).map(|(index, (nullifier, leaf))| {
        let (ephemeral_key, enc_ciphertext) = detection(txid, pool, index);
        OrchardAction { nullifier, cmx: commitment(leaf), ephemeral_key, enc_ciphertext }
    });
    actions.collect()
}

/// `OP_DUP OP_HASH160 <hash> OP_EQUALVERIFY OP_CHECKSIG`
pub fn p2pkh(hash: [u8; 20]) -> Script {
    Script::new([&[0x76, 0xa9, 0x14][..], &hash, &[0x88, 0xac]].concat())
}

pub fn outpoint(txid: [u8; 32], vout: u32) -> OutPoint {
    OutPoint { txid: TransactionId::from(txid), vout }
}

fn commitment(leaf: u32) -> NoteCommitment {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&leaf.to_le_bytes());
    NoteCommitment::from(bytes)
}

/// Builder-owned: distinct per (txid, pool, index), never asserted on
fn detection(
    txid: TransactionId,
    pool: ShieldedPool,
    index: u32,
) -> (EphemeralKey, CompactCiphertext) {
    let pool = pool.to_string();
    let seed = [&<[u8; 32]>::from(txid)[..], pool.as_bytes(), &index.to_le_bytes()].concat();
    let key = sha256d(&seed);
    let (head, tail) = (sha256d(&key), sha256d(&sha256d(&key)));
    let ciphertext: [u8; CompactCiphertext::LENGTH] =
        core::array::from_fn(|i| if i < 32 { head[i] } else { tail[i - 32] });
    (EphemeralKey::from(key), CompactCiphertext::from(ciphertext))
}
