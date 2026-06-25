//! Traits and types for the blockchain source thats serves zaino, commonly a validator connection.

use std::{error::Error, str::FromStr as _, sync::Arc};

use crate::chain_index::{
    types::{BlockHash, TransactionHash},
    ShieldedPool,
};
use async_trait::async_trait;
use futures::{future::join, TryFutureExt as _};
use incrementalmerkletree::frontier::CommitmentTree;
use tower::{Service, ServiceExt as _};
use zaino_common::Network;
use zaino_fetch::jsonrpsee::{
    connector::{JsonRpSeeConnector, RpcRequestError},
    response::{
        address_deltas::{GetAddressDeltasParams, GetAddressDeltasResponse},
        GetBlockError, GetBlockResponse, GetTransactionResponse, GetTreestateResponse,
    },
};
use zcash_primitives::merkle_tree::{read_commitment_tree, write_commitment_tree};
use zebra_chain::{
    block::TryIntoHeight, serialization::ZcashDeserialize, subtree::NoteCommitmentSubtreeIndex,
};
use zebra_rpc::{
    client::{GetAddressBalanceRequest, GetAddressTxIdsRequest},
    methods::{AddressBalance, GetAddressUtxos},
};
use zebra_state::{HashOrHeight, ReadRequest, ReadResponse, ReadStateService};

#[cfg(test)]
pub(crate) mod mockchain_source;

pub mod validator_connector;
pub use validator_connector::*;

/// A trait for accessing blockchain data from different backends.
///
/// TODO: Explore whether this should be split into separate capability based traits.
#[async_trait]
pub trait BlockchainSource: Clone + Send + Sync + 'static {
    // ********** Block methods **********

    /// Returns a best-chain block by hash or height
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>>;

    // ********** Transaction methods **********

    /// Returns the transaction by txid
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    >;

    /// Returns the complete list of txids currently in the mempool.
    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>>;

    // ********** Chain methods **********

    /// Returns the hash of the block at the tip of the best chain.
    async fn get_best_block_hash(&self)
        -> BlockchainSourceResult<Option<zebra_chain::block::Hash>>;

    /// Returns the height of the block at the tip of the best chain.
    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>>;

    /// Returns the sapling and orchard treestate by hash
    async fn get_treestate(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)>;

    /// Gets the subtree roots of a given pool and the end heights of each root,
    /// starting at the provided index, up to an optional maximum number of roots.
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>>;

    /// Returns the block commitment tree data by hash
    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )>;

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
    async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> BlockchainSourceResult<GetAddressDeltasResponse>;

    /// Returns the total balance of a provided `addresses` in an [`AddressBalance`] instance.
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
    async fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<AddressBalance>;

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
    async fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> BlockchainSourceResult<Vec<TransactionHash>>;

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
    async fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<Vec<GetAddressUtxos>>;

    // ********** Utility methods **********

    /// Get a listener for new nonfinalized blocks,
    /// if supported
    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn Error + Send + Sync>,
    >;

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
    /// Unrecoverable error.
    // TODO: Add logic for handling recoverable errors if any are identified
    // one candidate may be ephemerable network hiccoughs
    #[error("critical error in backing block source: {0}")]
    Unrecoverable(String),
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
