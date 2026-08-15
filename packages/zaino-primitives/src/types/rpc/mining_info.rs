//! `getmininginfo` — network mining statistics.

use crate::types::{Difficulty, Height};

/// Network mining statistics reported by the validator.
///
/// # Fields not modelled
///
/// The zcashd response carries a group of fields describing the *local mining
/// daemon* — `genproclimit`, `localsolps`, `generate`, `errorstimestamp` — plus
/// `pooledtx`. None are modelled here. Zaino is an indexer: it does not mine, so
/// a local miner's thread limit, solution rate, and on/off state describe a
/// component Zaino does not have and cannot report on meaningfully. `pooledtx`
/// duplicates a mempool count Zaino serves authoritatively from its own mempool
/// via `getmempoolinfo`, where it is coherent with the rest of Zaino's mempool
/// view; taking the validator's number here would let the two disagree. Zebra
/// reports none of these fields.
///
/// Fields a validator sends that are not listed below are dropped at the
/// adapter boundary — see the [module docs](super).
#[derive(Debug, Clone, PartialEq)]
pub struct MiningInfo {
    /// Height of the current best-chain tip.
    pub tip_height: Height,

    /// Name of the chain being served, e.g. `"main"`, `"test"`, `"regtest"`.
    ///
    /// This, not [`Self::testnet`], is the reliable network discriminator: it
    /// distinguishes regtest from testnet, which a boolean cannot.
    pub chain: String,

    /// Whether the validator considers itself to be on a test network.
    ///
    /// Retained because every known validator reports it, but prefer
    /// [`Self::chain`] — this collapses testnet and regtest into one value.
    pub testnet: bool,

    /// Size in bytes of the last block the validator built.
    ///
    /// `None` from validators that do not track block construction.
    pub current_block_size: Option<u64>,

    /// Transaction count in the last block the validator built.
    ///
    /// `None` from validators that do not track block construction.
    pub current_block_tx: Option<u64>,

    /// Estimated network solution rate, in solutions per second.
    ///
    /// `None` when the validator does not report `networksolps`.
    pub network_solution_rate: Option<u64>,

    /// Estimated network hash rate, in hashes per second.
    ///
    /// `None` when the validator does not report `networkhashps`. Reported
    /// separately from [`Self::network_solution_rate`] because the two are
    /// distinct measures under Equihash, not unit conversions of each other.
    pub network_hash_rate: Option<u64>,

    /// Current difficulty as a multiple of the network minimum.
    ///
    /// `None` from validators that omit it — Zebra serves difficulty through
    /// `getdifficulty` instead.
    pub difficulty: Option<Difficulty>,

    /// Validator status or error message, when there is one.
    ///
    /// `None` means no error. The Zcash RPC interface signals "healthy" with a
    /// sentinel rather than by omission — an empty string here, the literal
    /// `"no errors"` in `getinfo` — and the adapter normalises both to `None`,
    /// so a consumer tests `is_some()` instead of knowing each method's
    /// sentinel. See [`NodeInfo::errors`](super::NodeInfo::errors).
    pub errors: Option<String>,
}
