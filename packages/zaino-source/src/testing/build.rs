//! What a test states about a block or a transaction
//!
//! - [`MockChain`](super::MockChain) mints the rest into real consensus bytes (`mint.rs`)
//! - and checks it (`ledger.rs`)

use zcash_protocol::ShieldedPool;
use zcash_script::script::Code;
use zcash_transparent::address::Script;
use zcash_transparent::bundle::OutPoint;

/// One block: coinbase in slot 0, then each `tx` / `raw_tx` in call order
#[derive(Debug, Clone)]
pub struct BlockBuilder {
    pub(super) coinbase: TxBuilder,
    pub(super) txs: Vec<Planned>,
    pub(super) time: Option<u32>,
    pub(super) bits: Option<u32>,
}

#[derive(Debug, Clone)]
pub(super) enum Planned {
    Built(TxBuilder),
    Raw(Vec<u8>),
}

impl BlockBuilder {
    pub(super) fn new() -> Self {
        Self { coinbase: TxBuilder::new(), txs: Vec::new(), time: None, bits: None }
    }

    /// Default: pays nobody
    pub fn coinbase(self, tx: impl FnOnce(TxBuilder) -> TxBuilder) -> Self {
        Self { coinbase: tx(self.coinbase), ..self }
    }

    pub fn tx(mut self, tx: impl FnOnce(TxBuilder) -> TxBuilder) -> Self {
        self.txs.push(Planned::Built(tx(TxBuilder::new())));
        self
    }

    /// A transaction's bytes as written elsewhere (`raw_transaction`, a mempool entry re-mined)
    pub fn raw_tx(mut self, raw: Vec<u8>) -> Self {
        self.txs.push(Planned::Raw(raw));
        self
    }

    /// Header rule tests (default: parent + target spacing, must pass the median time past)
    pub fn time(self, unix: u32) -> Self {
        Self { time: Some(unix), ..self }
    }

    /// Header rule tests under `Work::Varied` (default: the regtest limit)
    pub fn bits(self, bits: u32) -> Self {
        Self { bits: Some(bits), ..self }
    }

    /// Anything a branch's past could refuse (a coinbase paying out: nothing)
    pub(super) fn reads_history(&self) -> bool {
        let coinbase = &self.coinbase;
        let actions = !coinbase.orchard_actions.is_empty() || !coinbase.ironwood_actions.is_empty();
        !self.txs.is_empty() || actions
    }
}

/// Where a spend's prevout comes from: mined before, or an earlier slot of the same block
#[derive(Debug, Clone)]
pub(super) enum Spend {
    Mined(OutPoint),
    InBlock { slot: usize, vout: u32 },
}

/// One transaction: values in zats, shielded notes as small tags (`mint::nullifier`,
/// `mint::commitment`)
#[derive(Debug, Clone)]
pub struct TxBuilder {
    pub(super) spends: Vec<Spend>,
    pub(super) pays: Vec<(Script, u64)>,
    pub(super) sapling_spends: Vec<u32>,
    pub(super) sapling_outputs: Vec<u32>,
    pub(super) orchard_actions: Vec<(u32, u32)>,
    pub(super) ironwood_actions: Vec<(u32, u32)>,
    pub(super) sprout_balance: i64,
    pub(super) sapling_balance: i64,
    pub(super) orchard_balance: i64,
    pub(super) ironwood_balance: i64,
    pub(super) fee: Option<u64>,
}

impl TxBuilder {
    pub(super) fn new() -> Self {
        Self {
            spends: Vec::new(),
            pays: Vec::new(),
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
            orchard_actions: Vec::new(),
            ironwood_actions: Vec::new(),
            sprout_balance: 0,
            sapling_balance: 0,
            orchard_balance: 0,
            ironwood_balance: 0,
            fee: None,
        }
    }

    /// A mined output (`chain.output(block, slot, vout)`)
    pub fn spend(mut self, prevout: OutPoint) -> Self {
        self.spends.push(Spend::Mined(prevout));
        self
    }

    /// Output `vout` of an earlier transaction in this block (`slot` 0 = its coinbase)
    pub fn spend_in_block(mut self, slot: usize, vout: u32) -> Self {
        self.spends.push(Spend::InBlock { slot, vout });
        self
    }

    pub fn pay(mut self, script: &Script, zats: u64) -> Self {
        self.pays.push((script.clone(), zats));
        self
    }

    pub fn sapling_spend(mut self, nullifier: u32) -> Self {
        self.sapling_spends.push(nullifier);
        self
    }

    pub fn sapling_output(mut self, leaf: u32) -> Self {
        self.sapling_outputs.push(leaf);
        self
    }

    pub fn orchard_action(mut self, nullifier: u32, leaf: u32) -> Self {
        self.orchard_actions.push((nullifier, leaf));
        self
    }

    pub fn ironwood_action(mut self, nullifier: u32, leaf: u32) -> Self {
        self.ironwood_actions.push((nullifier, leaf));
        self
    }

    /// + = out of `pool` into the transparent value pool (needs a description in that pool)
    pub fn value_balance(self, pool: ShieldedPool, zats: i64) -> Self {
        match pool {
            ShieldedPool::Sapling => Self { sapling_balance: zats, ..self },
            ShieldedPool::Orchard => Self { orchard_balance: zats, ..self },
            ShieldedPool::Ironwood => Self { ironwood_balance: zats, ..self },
        }
    }

    /// + = out of Sprout into the transparent value pool (one JoinSplit: a v4 transaction)
    pub fn sprout_balance(self, zats: i64) -> Self {
        Self { sprout_balance: zats, ..self }
    }

    /// Asserted against conservation when mined (unstated = derived)
    pub fn fee(self, zats: u64) -> Self {
        Self { fee: Some(zats), ..self }
    }
}

/// `OP_DUP OP_HASH160 <hash> OP_EQUALVERIFY OP_CHECKSIG`
pub fn p2pkh(hash: [u8; 20]) -> Script {
    Script(Code([&[0x76, 0xa9, 0x14][..], &hash, &[0x88, 0xac]].concat()))
}
