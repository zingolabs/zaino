//! Validator chain state, including the network upgrade schedule Zaino adopts.

use super::{
    BlockHash, ChainWork, ConsensusBranchIds, Difficulty, Height, NetworkUpgradeInfo,
    SignedZatoshis, Zatoshis,
};

/// The backing validator's view of the chain.
///
/// This is a domain type rather than one of the proxied
/// [`rpc`](super::rpc) shapes, because Zaino *consumes* it as well as
/// forwarding it: [`Self::upgrades`] is where Zaino learns the activation
/// schedule for the network it is serving, instead of relying on a compiled-in
/// one that could disagree with the validator.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockchainInfo {
    /// Network name as defined in BIP70 — `"main"`, `"test"`, `"regtest"`.
    pub chain: String,

    /// Number of blocks the validator has fully processed.
    pub blocks: Height,

    /// Height of the best header chain the validator has validated.
    ///
    /// Ahead of [`Self::blocks`] while it is still downloading block bodies.
    pub headers: Height,

    /// Height the validator estimates the network tip to be at.
    ///
    /// An estimate even when synced, so never treat it as authoritative;
    /// compare [`Self::blocks`] against it to gauge sync progress.
    pub estimated_height: Height,

    /// Hash of the current best block.
    pub best_block_hash: BlockHash,

    /// Current difficulty, as a multiple of the network minimum.
    pub difficulty: Difficulty,

    /// Verification progress relative to the estimated network tip, in `0.0..=1.0`.
    pub verification_progress: f64,

    /// Total work in the best chain.
    ///
    /// `None` when the validator does not track it. Zebra does not store
    /// cumulative work per height (ZcashFoundation/zebra#7109) and reports zero
    /// — which is not a possible amount of work for a real chain, so it is
    /// carried as absence rather than as the number zero, which a consumer
    /// might otherwise compare against.
    ///
    /// Full 256-bit width where it is reported. The wire form is a 64-bit
    /// integer upstream despite documenting itself as hex-encoded, which would
    /// truncate every mainnet value; [`ChainWork`] avoids that.
    pub chain_work: Option<ChainWork>,

    /// Whether the validator has pruned block data.
    pub pruned: bool,

    /// Approximate on-disk size of the validator's block and undo data, in bytes.
    pub size_on_disk: u64,

    /// Total note commitments across the shielded pools.
    pub commitments: u64,

    /// Total transparent and shielded value on the chain.
    pub chain_supply: ValuePoolBalance,

    /// Per-pool value balances.
    pub value_pools: Vec<ValuePoolBalance>,

    /// The validator's network upgrade schedule.
    ///
    /// Load-bearing: Zaino derives its runtime activation heights from this, so
    /// it is a consensus input, not a diagnostic. Ordered as the validator
    /// reported it.
    pub upgrades: Vec<NetworkUpgradeInfo>,

    /// Consensus branches in force at the tip and for the next block.
    pub consensus: ConsensusBranchIds,
}

/// The balance held in one value pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValuePoolBalance {
    /// Pool name, e.g. `"transparent"`, `"sapling"`, `"orchard"`.
    pub id: String,

    /// Total value currently in the pool.
    ///
    /// Zatoshis only. The wire form reports every amount twice — once as a
    /// ZEC-denominated float and once in zatoshis — which is redundant and
    /// invites the two to disagree; only the exact integer is kept.
    pub chain_value: Zatoshis,

    /// Whether the validator is tracking this pool's balance.
    ///
    /// When `false`, [`Self::chain_value`] is not meaningful.
    pub monitored: bool,

    /// Change to the pool's balance produced by the latest block.
    ///
    /// `None` when the validator does not report a delta. Signed: value leaves
    /// a pool as well as entering it.
    pub value_delta: Option<SignedZatoshis>,
}
