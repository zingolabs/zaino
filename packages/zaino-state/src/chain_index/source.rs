//! ChainIndex's driven port onto the backing validator.
//!
//! # This is temporary scaffolding
//!
//! [`BlockchainSource`] is not the abstraction Zaino wants over a validator. It
//! is declared in the transport's vocabulary — its methods return
//! `zebra_chain` and `zebra_rpc` types — so anything depending on
//! it inherits that whole graph. That is why no subsystem could be extracted
//! from `zaino-state` without dragging those crates along, and it is the reason
//! the `zaino-source` ports exist.
//!
//! The real port layer now lives in `zaino-source`: one trait per question a
//! consumer can ask, in domain vocabulary, with per-query errors. The composite
//! in `zaino-source-zebra` routes each question to whichever transport can
//! answer it.
//!
//! This trait survives only as an **anti-corruption layer**, so that ChainIndex
//! and everything above it keep working while the new stack is wired in
//! underneath. Its single implementation,
//! [`ZebraValidatorSource`](crate::chain_index::validator_source::ZebraValidatorSource),
//! delegates to that composite and converts back into the shapes these
//! signatures still demand.
//!
//! **Do not extend it.** A new capability belongs in `zaino-source`, where it
//! can be expressed as its own question and implemented only by the transports
//! that can answer it. This module shrinks as each ChainIndex subsystem is
//! isolated onto the real ports, and is deleted with the last of them.
//!
//! # How it shrinks: methods become supertraits
//!
//! `zaino-state` is being taken apart one subsystem at a time. Each moves into
//! its own crate and is wired back in here with the *smallest* change to the
//! surrounding code, so the extraction never has to happen at the same time as
//! a rewrite of every caller. The mempool has gone (`zaino-mempool` /
//! `zaino-mempool-service`); the non-finalised state and the finalised state
//! follow; then a new ChainHead crate replaces `ChainIndex` and this crate
//! retires.
//!
//! The mechanism, and the thing that is easy to misread: **before** a subsystem
//! migrates, its needs sit on this trait as wire-typed *methods*; **after**, they
//! sit on it as `zaino-source` *supertraits*. The requirement does not
//! disappear — ChainIndex still has to hand the subsystem a source that can
//! answer it — but the answering stops being routed through here.
//!
//! So a growing supertrait list is this trait *dissolving*, not accreting. Each
//! migration converts method-surface into port-surface, and the trait tends
//! toward a bound with no methods of its own, at which point deleting it at the
//! ChainHead cutover is mechanical. A subsystem bounded on `BlockchainSource`
//! but calling none of its methods — as the mempool wiring now is — is the
//! finished state for that subsystem, not a subsystem that failed to leave.
//!
//! Two things to notice when the last of it goes:
//!
//! - A migrated subsystem's ports are named **twice**: here as supertraits, and
//!   in `ChainIndexSourcePorts` (see the sibling `source_ports` module), which
//!   is what `ValidatorSource`'s type parameter is bounded on. They sit at
//!   different levels, so this is not redundant today — but nothing enforces
//!   that they agree, so a new port has to be added in both places.
//! - Because of that, the end state converges on this trait being substantially
//!   a duplicate of `ChainIndexSourcePorts`. That convergence is the signal that
//!   both are ready to delete, rather than something to reconcile earlier.

use std::{error::Error, sync::Arc};

use crate::chain_index::{
    types::{BlockHash, TransactionHash},
    ShieldedPool,
};
use crate::SendFut;
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, MiningInfo,
    NodeInfo, PeerInfo,
};
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};
use zebra_state::HashOrHeight;

#[cfg(test)]
pub(crate) mod mockchain_source;

/// One pool's treestate for a block.
///
/// Re-exported from the domain vocabulary so the scaffolding port and the
/// driving ports name the same type.
pub use zaino_primitives::types::PoolTreestate;

/// Per-pool treestates `(sapling, orchard, ironwood)`, each `None` when the pool has no
/// treestate at the queried block.
pub(crate) type TreestateBytes = (
    Option<PoolTreestate>,
    Option<PoolTreestate>,
    Option<PoolTreestate>,
);

/// Sapling and orchard note-commitment tree roots `(sapling, orchard, ironwood)`, each
/// paired with its tree size; `None` when the pool has no root at the block.
pub(crate) type ShieldedTreeRoots = (
    Option<(zebra_chain::sapling::tree::Root, u64)>,
    Option<(zebra_chain::orchard::tree::Root, u64)>,
    Option<(zebra_chain::orchard::tree::Root, u64)>,
);

/// Receiver for newly observed nonfinalized blocks, delivered as `(hash, block)`.
pub(crate) type NonfinalizedBlockReceiver =
    tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>;

/// ChainIndex's driven port onto the backing validator.
///
/// Temporary scaffolding — see the [module docs](self). The capability-based
/// split this once carried a TODO for now exists in `zaino-source`; this trait
/// remains only so ChainIndex can keep its current shape while the new stack is
/// wired in beneath it.
///
/// # The mempool ports are supertraits, not methods
///
/// The four mempool questions are required here rather than declared as methods
/// below, because the mempool subsystem has already completed the migration this
/// trait is scaffolding for: it reads `zaino-source` directly and never sees a
/// `BlockchainSource`. Restating those questions as wire-typed methods would
/// mean converting domain types out and back for no reader.
///
/// This is what "methods leave `BlockchainSource` as subsystems move onto the
/// ports" looks like from the other side — the requirement stays (ChainIndex
/// still needs a source that can answer them, to hand to the mempool), but the
/// answering is no longer routed through here. See the [module docs](self) for
/// why a growing supertrait list is this trait dissolving rather than accreting.
///
/// A consequence worth stating, because it looks like a defect and is not:
/// `MempoolSourceAdapter` (in the sibling `mempool` module) is bounded on this
/// trait while calling none of its methods — it only forwards the supertraits
/// above. That is the migrated state. Narrowing its bound to those four ports
/// would decouple nothing on its own, because ChainIndex only ever instantiates
/// it from its own `Source: BlockchainSource`; the change that decouples is
/// removing the supertraits, which pushes the requirement onto every
/// `NodeBackedChainIndex` bound and is churn this staging exists to avoid until
/// the ChainHead cutover does it once.
pub trait BlockchainSource:
    zaino_source::GetMempoolTxids
    + zaino_source::GetMempoolMetadata
    + zaino_source::GetRawMempoolTransaction
    + zaino_source::GetMempoolSourceTip
    + Clone
    + Send
    + Sync
    + 'static
{
    // ********** Block methods **********

    /// Returns a best-chain block by hash or height
    fn get_block(
        &self,
        id: HashOrHeight,
    ) -> impl SendFut<BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>>>;

    /// Returns the `getblock`-shaped verbose block for the given hash or height.
    ///
    /// `verbosity` follows the zcashd `getblock` convention (0 = raw, 1 = object with
    /// txids, 2 = object with full transaction data).
    fn get_block_verbose(
        &self,
        hash_or_height: HashOrHeight,
        verbosity: Option<u8>,
    ) -> impl SendFut<BlockchainSourceResult<zebra_rpc::methods::GetBlock>>;

    /// Returns the `getblockheader`-shaped block header for the given block hash.
    ///
    /// When `verbose` is false the header is returned in raw hex form; when true it is
    /// returned as a structured object.
    fn get_block_header(
        &self,
        hash: String,
    ) -> impl SendFut<BlockchainSourceResult<BlockHeaderVerbose>>;

    /// Returns the raw serialised header of the block with the given hash.
    ///
    /// The non-verbose half of `getblockheader`. Verbosity is chosen by the
    /// caller, so it selects between two questions here rather than making one
    /// answer polymorphic; the serving layer picks which to ask.
    fn get_raw_block_header(&self, hash: String) -> impl SendFut<BlockchainSourceResult<Vec<u8>>>;

    /// Returns the `getblockdeltas`-shaped transparent input/output deltas for the block
    /// with the given hash.
    fn get_block_deltas(&self, hash: String) -> impl SendFut<BlockchainSourceResult<BlockDeltas>>;

    // ********** Transaction methods **********

    /// Returns the transaction by txid
    fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> impl SendFut<
        BlockchainSourceResult<
            Option<(
                Arc<zebra_chain::transaction::Transaction>,
                GetTransactionLocation,
            )>,
        >,
    >;

    // ********** Chain methods **********

    /// Returns the hash of the block at the tip of the best chain.
    fn get_best_block_hash(
        &self,
    ) -> impl SendFut<BlockchainSourceResult<Option<zebra_chain::block::Hash>>>;

    /// Returns the height of the block at the tip of the best chain.
    fn get_best_block_height(
        &self,
    ) -> impl SendFut<BlockchainSourceResult<Option<zebra_chain::block::Height>>>;

    /// Returns the proof-of-work difficulty of the best chain as a multiple of the
    /// minimum difficulty (the `getdifficulty` RPC value).
    fn get_difficulty(&self) -> impl SendFut<BlockchainSourceResult<f64>>;

    /// A watch stream of the source's chain-tip changes, when the source owns
    /// one locally. Only the `Direct` validator connection (which drives its
    /// own Zebra syncer) does; every other source observes tips by polling,
    /// and inherits this `None` default.
    fn chain_tip_change(&self) -> Option<zebra_state::ChainTipChange> {
        None
    }

    /// Returns the `getblockchaininfo` response.
    fn get_blockchain_info(
        &self,
    ) -> impl SendFut<BlockchainSourceResult<zaino_primitives::types::BlockchainInfo>>;

    // ********** Node-passthrough methods **********
    //
    // These have no local-index equivalent and always proxy to the backing validator's
    // JSON-RPC interface.

    /// Returns the `getinfo` response.
    fn get_info(&self) -> impl SendFut<BlockchainSourceResult<NodeInfo>>;

    /// Returns the `getpeerinfo` response.
    fn get_peer_info(&self) -> impl SendFut<BlockchainSourceResult<Vec<PeerInfo>>>;

    /// Returns the validator's `getchaintips` response. Serves as the
    /// `getchaintips` fallback while the local index is still building its
    /// finalised state and has no non-finalised snapshot to answer from.
    fn get_chain_tips(
        &self,
    ) -> impl SendFut<BlockchainSourceResult<Vec<zaino_primitives::types::rpc::ChainTip>>>;

    /// Returns the `getblocksubsidy` response at the given height.
    fn get_block_subsidy(&self, height: u32) -> impl SendFut<BlockchainSourceResult<BlockSubsidy>>;

    /// Returns the `getmininginfo` response.
    fn get_mining_info(&self) -> impl SendFut<BlockchainSourceResult<MiningInfo>>;

    /// Returns the `gettxout` response for the given outpoint.
    fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> impl SendFut<BlockchainSourceResult<Option<zaino_primitives::types::rpc::TxOut>>>;

    /// Returns the `getspentinfo` response for the given request.
    fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> impl SendFut<BlockchainSourceResult<zaino_primitives::types::rpc::SpentInfo>>;

    /// Returns the `getnetworksolps` response.
    fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> impl SendFut<BlockchainSourceResult<u64>>;

    /// Submits a raw transaction to the network via the validator's mempool
    /// (`sendrawtransaction`).
    fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> impl SendFut<BlockchainSourceResult<zaino_primitives::types::TransactionId>>;

    /// Returns the full `z_gettreestate` response for the given hash-or-height string.
    ///
    /// Node-passthrough fallback for treestates not locally serviceable by the index.
    fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> impl SendFut<BlockchainSourceResult<zaino_primitives::types::Treestate>>;

    /// Returns the sapling and orchard treestate by hash
    fn get_treestate(&self, id: BlockHash) -> impl SendFut<BlockchainSourceResult<TreestateBytes>>;

    /// Gets the subtree roots of a given pool and the end heights of each root,
    /// starting at the provided index, up to an optional maximum number of roots.
    fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> impl SendFut<BlockchainSourceResult<Vec<([u8; 32], u32)>>>;

    /// Returns the block commitment tree data by hash
    fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> impl SendFut<BlockchainSourceResult<ShieldedTreeRoots>>;

    // ********** Transparent address methods **********

    /// Returns all changes for an address.
    ///
    /// Returns information about all changes to the given transparent addresses within the given (inclusive)
    ///
    /// block height range, default is the full blockchain.
    /// If start or end are not specified, they default to zero.
    /// If start is greater than the latest block height, it's interpreted as that height.
    ///
    /// If end is zero, it's interpreted as the latest block height.
    ///
    /// [Original zcashd implementation](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/misc.cpp#L881)
    ///
    /// zcashd reference: [`getaddressdeltas`](https://zcash.github.io/rpc/getaddressdeltas.html)
    /// method: post
    /// tags: address
    fn get_address_deltas(
        &self,
        params: AddressDeltasRequest,
    ) -> impl SendFut<BlockchainSourceResult<AddressDeltas>>;

    /// Returns the total balance of a provided `addresses` in an [`AddressBalance`](zaino_primitives::types::AddressBalance) instance.
    ///
    /// zcashd reference: [`getaddressbalance`](https://zcash.github.io/rpc/getaddressbalance.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `address_strings`: (object, example={"addresses": ["tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ"]}) A JSON map with a single entry
    ///     - `addresses`: (array of strings) A list of base-58 encoded addresses.
    ///
    /// # Notes
    ///
    /// zcashd also accepts a single string parameter instead of an array of strings, but Zebra
    /// doesn't because lightwalletd always calls this RPC with an array of addresses.
    ///
    /// zcashd also returns the total amount of Zatoshis received by the addresses, but Zebra
    /// doesn't because lightwalletd doesn't use that information.
    ///
    /// The RPC documentation says that the returned object has a string `balance` field, but
    /// zcashd actually [returns an
    /// integer](https://github.com/zcash/lightwalletd/blob/bdaac63f3ee0dbef62bde04f6817a9f90d483b00/common/common.go#L128-L130).
    fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> impl SendFut<BlockchainSourceResult<zaino_primitives::types::AddressBalance>>;

    /// Returns the transaction ids made by the provided transparent addresses.
    ///
    /// zcashd reference: [`getaddresstxids`](https://zcash.github.io/rpc/getaddresstxids.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `request`: (object, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"], \"start\": 1000, \"end\": 2000}) A struct with the following named fields:
    ///     - `addresses`: (json array of string, required) The addresses to get transactions from.
    ///     - `start`: (numeric, required) The lower height to start looking for transactions (inclusive).
    ///     - `end`: (numeric, required) The top height to stop looking for transactions (inclusive).
    ///
    /// # Notes
    ///
    /// Only the multi-argument format is used by lightwalletd and this is what we currently support:
    /// <https://github.com/zcash/lightwalletd/blob/631bb16404e3d8b045e74a7c5489db626790b2f6/common/common.go#L97-L102>
    fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> impl SendFut<BlockchainSourceResult<Vec<TransactionHash>>>;

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// zcashd reference: [`getaddressutxos`](https://zcash.github.io/rpc/getaddressutxos.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `addresses`: (array, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"]}) The addresses to get outputs from.
    ///
    /// # Notes
    ///
    /// lightwalletd always uses the multi-address request, without chaininfo:
    /// <https://github.com/zcash/lightwalletd/blob/master/frontend/service.go#L402>
    fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> impl SendFut<BlockchainSourceResult<Vec<zaino_primitives::types::Utxo>>>;

    // ********** Utility methods **********

    /// Get a listener for new nonfinalized blocks,
    /// if supported
    fn nonfinalized_listener(
        &self,
    ) -> impl SendFut<Result<Option<NonfinalizedBlockReceiver>, Box<dyn Error + Send + Sync>>>;

    /// Subscribe to "blocks received at the source" notifications.
    ///
    /// Returns a `tokio::sync::watch::Receiver<()>` — the idiomatic Tokio
    /// "wake-on-change" primitive. The transport coalesces by construction:
    /// any number of `send_replace(())` calls on the sender side between
    /// two `changed().await` calls on the receiver side collapse into a
    /// single wake. Subscribers re-read source state on each wake, so the
    /// consumer cares only about *whether* new blocks arrived, not *how
    /// many* events fired.
    ///
    /// Sync loops typically call this once at startup and `select!`
    /// `changed()` against their fixed-cadence timer, falling through to
    /// the timer when no push notification arrives.
    ///
    /// Default returns `None` — poll-only sources (real validators) pace
    /// themselves on the timer alone. Push-capable sources (test
    /// mockchains) override to provide a live receiver.
    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        None
    }

    /// Release any long-lived resources the source owns (e.g. a background
    /// syncer task feeding a `ReadStateService`).
    ///
    /// Default is a no-op — poll-only sources (the RPC adapter) and test
    /// mockchains own nothing to tear down. Sources that spawn their own
    /// validator plumbing (the read-state adapter, which owns
    /// the Zebra syncer task) override this to abort that task on shutdown.
    fn shutdown(&self) {}
}

/// Sleep up to `duration`, but return early if `change_rx` resolves first.
///
/// Sync loops in this module pace themselves on a fixed-cadence timer and
/// want to wake immediately when the source signals new state. The two-arm
/// `tokio::select!` is identical at every call site; this helper is the
/// single home for the pattern. Pass `None` for poll-only sources — the
/// helper degrades to a plain sleep.
pub(super) async fn wait_or_source_change(
    change_rx: Option<&mut tokio::sync::watch::Receiver<()>>,
    duration: std::time::Duration,
) {
    match change_rx {
        Some(rx) => tokio::select! {
            _ = tokio::time::sleep(duration) => {}
            _ = rx.changed() => {}
        },
        None => tokio::time::sleep(duration).await,
    }
}

// ********** Error / data types + helper methods **********
// NOTE: Should these be moved into error / type modules?

/// An error originating from a blockchain source.
#[derive(Debug, thiserror::Error)]
pub enum BlockchainSourceError {
    /// Unrecoverable error described only by a message (no underlying
    /// typed error exists, e.g. an unexpected response shape).
    // TODO: Add logic for handling recoverable errors if any are identified
    // one candidate may be ephemerable network hiccoughs
    #[error("critical error in backing block source: {0}")]
    Unrecoverable(String),
    /// Unrecoverable error whose typed cause is preserved as
    /// [`std::error::Error::source`]. zaino-serve recovers zcashd-compatible
    /// RPC error codes by downcast-walking `source()` chains, so errors that
    /// wrap a typed transport or RPC error must use this variant rather than
    /// [`Self::Unrecoverable`] with a stringified cause.
    #[error("critical error in backing block source: {message}")]
    UnrecoverableWithSource {
        /// Rendered description, including the cause's `Display` output so
        /// top-level log lines match the previous stringified form.
        message: String,
        /// The typed cause, available to `source()`-chain walks.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl BlockchainSourceError {
    /// Wraps a typed error, preserving it for `source()`-chain recovery.
    /// Accepts both concrete error types and already-boxed errors (e.g.
    /// zebra's `BoxError`).
    pub(crate) fn unrecoverable(
        error: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        let source = error.into();
        Self::UnrecoverableWithSource {
            message: source.to_string(),
            source,
        }
    }

    /// Wraps a typed error with a context prefix, preserving the error for
    /// `source()`-chain recovery.
    pub(crate) fn unrecoverable_context(
        context: impl std::fmt::Display,
        error: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        let source = error.into();
        Self::UnrecoverableWithSource {
            message: format!("{context}: {source}"),
            source,
        }
    }
}

/// Error type returned when invalid data is returned by the validator.
#[derive(thiserror::Error, Debug)]
#[error("data from validator invalid: {0}")]
pub struct InvalidData(String);

pub(crate) type BlockchainSourceResult<T> = Result<T, BlockchainSourceError>;

/// The location of a transaction returned by
/// [BlockchainSource::get_transaction]
#[derive(Debug, Clone)]
pub enum GetTransactionLocation {
    // get_transaction can get the height of the block
    // containing the transaction if it's on the best
    // chain, but cannot reliably if it isn't.
    //
    /// The transaction is in the best chain,
    /// the block height is returned
    BestChain(zebra_chain::block::Height),
    /// The transaction is on a non-best chain
    NonbestChain,
    /// The transaction is in the mempool
    Mempool,
}
