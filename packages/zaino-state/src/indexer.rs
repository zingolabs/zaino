//! Zaino's indexer frontend: the `ZcashIndexer` / `LightWalletIndexer` RPC trait
//! definitions served by zaino, the generic [`IndexerService`] / [`IndexerSubscriber`]
//! wrappers, and the concrete [`node_backed_indexer::NodeBackedIndexerService`] — the
//! single validator-backed service (JSON-RPC `Rpc` or direct `ReadStateService`
//! connection, selected at runtime).

pub(crate) mod node_backed_indexer;

use crate::SendFut;
use tokio::{sync::mpsc, time::timeout};
use tracing::warn;
use zaino_fetch::jsonrpsee::response::{
    address_deltas::{GetAddressDeltasParams, GetAddressDeltasResponse},
    block_deltas::BlockDeltas,
    block_header::GetBlockHeader,
    block_subsidy::GetBlockSubsidy,
    chain_tips::GetChainTipsResponse,
    mining_info::GetMiningInfoWire,
    peer_info::GetPeerInfo,
    z_validate_address::{
        InvalidZValidateAddress, KnownZValidateAddress, ZValidateAddressResponse,
        DEPRECATION_NOTICE as Z_VALIDATE_DEPRECATION,
    },
    GetMempoolInfoResponse, GetNetworkSolPsResponse, GetSpentInfoRequest, GetSpentInfoResponse,
    GetSubtreesResponse, GetTxOutSetInfoResponse,
};
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, Balance, BlockId, BlockRange, Duration, GetAddressUtxosArg,
        GetAddressUtxosReplyList, GetMempoolTxRequest, GetSubtreeRootsArg, LightdInfo,
        PingResponse, RawTransaction, SendResponse, ShieldedProtocol, SubtreeRoot,
        TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};
use zebra_chain::{
    block::Height, serialization::BytesInDisplayOrder as _, subtree::NoteCommitmentSubtreeIndex,
};
use zebra_rpc::{
    client::{
        GetSubtreesByIndexResponse, GetTreestateResponse, SubtreeRpcData, ValidateAddressResponse,
    },
    methods::{
        AddressBalance, GetAddressBalanceRequest, GetAddressTxIdsRequest, GetAddressUtxos,
        GetBlock, GetBlockHash, GetBlockchainInfoResponse, GetInfo, GetRawTransaction,
        SentTransactionHash,
    },
};

use crate::{
    status::Status,
    stream::{
        AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
        SubtreeRootReplyStream, UtxoReplyStream,
    },
};

/// Wrapper struct for a ZainoState chain-fetch service (currently the single
/// [`node_backed_indexer::NodeBackedIndexerService`]).
///
/// The future plan is to also add a TonicService and DarksideService to this to enable
/// wallets to use a single unified chain fetch service.
#[derive(Clone)]
pub struct IndexerService<Service: ZcashService> {
    /// Underlying Service.
    service: Service,
}

impl<Service> IndexerService<Service>
where
    Service: ZcashService,
{
    /// Creates a new `IndexerService` using the provided `config`.
    pub async fn spawn(
        config: Service::Config,
    ) -> Result<Self, <Service::Subscriber as ZcashIndexer>::Error> {
        Ok(IndexerService {
            service: Service::spawn(config)
                .await
                .map_err(Into::<tonic::Status>::into)?,
        })
    }

    /// Returns a reference to the inner service.
    pub fn inner_ref(&self) -> &Service {
        &self.service
    }

    /// Consumes the `IndexerService` and returns the inner service.
    pub fn inner(self) -> Service {
        self.service
    }
}

/// Zcash Service functionality.
///
/// Implementors automatically gain [`Liveness`](zaino_common::probing::Liveness) and
/// [`Readiness`](zaino_common::probing::Readiness) via the [`Status`] supertrait.
pub trait ZcashService: Sized + Status {
    /// A subscriber to the service, used to fetch chain data.
    type Subscriber: Clone + ZcashIndexer + LightWalletIndexer + Status;

    /// Service Config.
    type Config: Clone;

    /// Spawns a [`ZcashIndexer`].
    fn spawn(
        config: Self::Config,
    ) -> impl SendFut<Result<Self, <Self::Subscriber as ZcashIndexer>::Error>>;

    /// Returns a [`IndexerSubscriber`].
    fn get_subscriber(&self) -> IndexerSubscriber<Self::Subscriber>;

    /// Shuts down the StateService.
    fn close(&mut self);
}

/// Wrapper struct for a ZainoState chain-fetch service subscriber (currently the single
/// [`node_backed_indexer::NodeBackedIndexerServiceSubscriber`]).
///
/// The future plan is to also add a TonicServiceSubscriber and DarksideServiceSubscriber to this to enable wallets to use a single unified chain fetch service.
#[derive(Clone)]
pub struct IndexerSubscriber<Subscriber: Clone + ZcashIndexer + LightWalletIndexer + Send + Sync> {
    /// Underlying Service Subscriber.
    subscriber: Subscriber,
}

impl<Subscriber> IndexerSubscriber<Subscriber>
where
    Subscriber: Clone + ZcashIndexer + LightWalletIndexer,
{
    /// Creates a new [`IndexerSubscriber`].
    pub fn new(subscriber: Subscriber) -> Self {
        IndexerSubscriber { subscriber }
    }

    /// Returns a reference to the inner service.
    pub fn inner_ref(&self) -> &Subscriber {
        &self.subscriber
    }

    /// Returns a clone of the inner service.
    pub fn inner_clone(&self) -> Subscriber {
        self.subscriber.clone()
    }

    /// Consumes the `IndexerService` and returns the inner service.
    pub fn inner(self) -> Subscriber {
        self.subscriber
    }
}

/// Zcash RPC method signatures.
///
/// Doc comments taken from Zebra for consistency.
pub trait ZcashIndexer: Send + Sync + 'static {
    /// Uses underlying error type of implementer.
    type Error: std::error::Error
        + From<tonic::Status>
        + Into<tonic::Status>
        + Send
        + Sync
        + 'static;

    /// Returns software information from the RPC server, as a [`GetInfo`] JSON struct.
    ///
    /// zcashd reference: [`getinfo`](https://zcash.github.io/rpc/getinfo.html)
    /// method: post
    /// tags: control
    ///
    /// # Notes
    ///
    /// [The zcashd reference](https://zcash.github.io/rpc/getinfo.html) might not show some fields
    /// in Zebra's [`GetInfo`]. Zebra uses the field names and formats from the
    /// [zcashd code](https://github.com/zcash/zcash/blob/v4.6.0-1/src/rpc/misc.cpp#L86-L87).
    ///
    /// Some fields from the zcashd reference are missing from Zebra's [`GetInfo`]. It only contains the fields
    /// [required for lightwalletd support.](https://github.com/zcash/lightwalletd/blob/v0.4.9/common/common.go#L91-L95)
    fn get_info(&self) -> impl SendFut<Result<GetInfo, Self::Error>>;

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
        params: GetAddressDeltasParams,
    ) -> impl SendFut<Result<GetAddressDeltasResponse, Self::Error>>;

    /// Returns blockchain state information, as a [`GetBlockchainInfoResponse`] JSON struct.
    ///
    /// zcashd reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Notes
    ///
    /// Some fields from the zcashd reference are missing from Zebra's [`GetBlockchainInfoResponse`]. It only contains the fields
    /// [required for lightwalletd support.](https://github.com/zcash/lightwalletd/blob/v0.4.9/common/common.go#L72-L89)
    fn get_blockchain_info(&self) -> impl SendFut<Result<GetBlockchainInfoResponse, Self::Error>>;

    /// Returns the proof-of-work difficulty as a multiple of the minimum difficulty.
    ///
    /// zcashd reference: [`getdifficulty`](https://zcash.github.io/rpc/getdifficulty.html)
    /// method: post
    /// tags: blockchain
    fn get_difficulty(&self) -> impl SendFut<Result<f64, Self::Error>>;

    /// Returns block subsidy reward, taking into account the mining slow start and the founders reward, of block at index provided.
    ///
    /// zcashd reference: [`getblocksubsidy`](https://zcash.github.io/rpc/getblocksubsidy.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `height`: (number, optional) The block height. If not provided, defaults to the current height of the chain.
    fn get_block_subsidy(&self, height: u32) -> impl SendFut<Result<GetBlockSubsidy, Self::Error>>;

    /// Returns details on the active state of the TX memory pool.
    ///
    /// zcashd reference: [`getmempoolinfo`](https://zcash.github.io/rpc/getmempoolinfo.html)
    /// method: post
    /// tags: mempool
    ///
    /// Original implementation: [`getmempoolinfo`](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/blockchain.cpp#L1555)
    fn get_mempool_info(&self) -> impl SendFut<Result<GetMempoolInfoResponse, Self::Error>>;

    /// Returns data about each connected network node as a json array of objects.
    ///
    /// zcashd reference: [`getpeerinfo`](https://zcash.github.io/rpc/getpeerinfo.html)
    /// tags: network
    ///
    /// Current `zebrad` does not include the same fields as `zcashd`.
    fn get_peer_info(&self) -> impl SendFut<Result<GetPeerInfo, Self::Error>>;

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
    fn z_get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> impl SendFut<Result<AddressBalance, Self::Error>>;

    /// Sends the raw bytes of a signed transaction to the local node's mempool, if the transaction is valid.
    /// Returns the [`SentTransactionHash`] for the transaction, as a JSON string.
    ///
    /// zcashd reference: [`sendrawtransaction`](https://zcash.github.io/rpc/sendrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `raw_transaction_hex`: (string, required, example="signedhex") The hex-encoded raw transaction bytes.
    ///
    /// # Notes
    ///
    /// zcashd accepts an optional `allowhighfees` parameter. Zebra doesn't support this parameter,
    /// because lightwalletd doesn't use it.
    fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> impl SendFut<Result<SentTransactionHash, Self::Error>>;

    /// If verbose is false, returns a string that is serialized, hex-encoded data for blockheader `hash`.
    /// If verbose is true, returns an Object with information about blockheader `hash`.
    ///
    /// # Parameters
    ///
    /// - hash: (string, required) The block hash
    /// - verbose: (boolean, optional, default=true) true for a json object, false for the hex encoded data
    ///
    /// zcashd reference: [`getblockheader`](https://zcash.github.io/rpc/getblockheader.html)
    /// method: post
    /// tags: blockchain
    fn get_block_header(
        &self,
        hash: String,
        verbose: bool,
    ) -> impl SendFut<Result<GetBlockHeader, Self::Error>>;

    /// Returns the requested block by hash or height, as a [`GetBlock`] JSON string.
    /// If the block is not in Zebra's state, returns
    /// [error code `-8`.](https://github.com/zcash/zcash/issues/5758) if a height was
    /// passed or -5 if a hash was passed.
    ///
    /// zcashd reference: [`getblock`](https://zcash.github.io/rpc/getblock.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash_or_height`: (string, required, example="1") The hash or height for the block to be returned.
    /// - `verbosity`: (number, optional, default=1, example=1) 0 for hex encoded data, 1 for a json object, and 2 for json object with transaction data.
    ///
    /// # Notes
    ///
    /// Zebra previously partially supported verbosity=1 by returning only the
    /// fields required by lightwalletd ([`lightwalletd` only reads the `tx`
    /// field of the result](https://github.com/zcash/lightwalletd/blob/dfac02093d85fb31fb9a8475b884dd6abca966c7/common/common.go#L152)).
    /// That verbosity level was migrated to "3"; so while lightwalletd will
    /// still work by using verbosity=1, it will sync faster if it is changed to
    /// use verbosity=3.
    ///
    /// The undocumented `chainwork` field is not returned.
    fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> impl SendFut<Result<GetBlock, Self::Error>>;

    /// Returns information about the given block and its transactions.
    ///
    /// zcashd reference: [`getblockdeltas`](https://zcash.github.io/rpc/getblockdeltas.html)
    /// method: post
    /// tags: blockchain
    fn get_block_deltas(&self, hash: String) -> impl SendFut<Result<BlockDeltas, Self::Error>>;

    /// Returns the current block count in the best valid block chain.
    ///
    /// zcashd reference: [`getblockcount`](https://zcash.github.io/rpc/getblockcount.html)
    /// method: post
    /// tags: blockchain
    fn get_block_count(&self) -> impl SendFut<Result<Height, Self::Error>>;

    /// Returns information about all known tips in the block tree.
    ///
    /// zcashd reference: [`getchaintips`](https://zcash.github.io/rpc/getchaintips.html)
    /// method: post
    /// tags: blockchain
    ///
    /// zcashd builds the response from all block-index leaves, always includes the active
    /// tip, sorts by descending height, and classifies leaves as `invalid`, `headers-only`,
    /// `valid-headers`, `valid-fork`, `active`, or `unknown`.
    fn get_chain_tips(&self) -> impl SendFut<Result<GetChainTipsResponse, Self::Error>>;

    /// Return information about the given Zcash address.
    ///
    /// # Parameters
    /// - `address`: (string, required, example="tmHMBeeYRuc2eVicLNfP15YLxbQsooCA6jb") The Zcash transparent address to validate.
    ///
    /// zcashd reference: [`validateaddress`](https://zcash.github.io/rpc/validateaddress.html)
    /// method: post
    /// tags: util
    fn validate_address(
        &self,
        address: String,
    ) -> impl SendFut<Result<ValidateAddressResponse, Self::Error>>;

    /// Return information about the given address.
    ///
    /// # Deprecation
    ///
    /// See [`z_validate_address::DEPRECATION_NOTICE`](zaino_fetch::jsonrpsee::response::z_validate_address::DEPRECATION_NOTICE).
    ///
    /// # Parameters
    /// - `address`: (string, required) The address to validate.
    ///
    /// zcashd reference: [`z_validateaddress`](https://zcash.github.io/rpc/z_validateaddress.html)
    /// method: post
    /// tags: util
    #[deprecated(note = "https://github.com/zingolabs/zaino/issues/992#issuecomment-4245596178")]
    fn z_validate_address(
        &self,
        address: String,
    ) -> impl SendFut<Result<ZValidateAddressResponse, Self::Error>>;

    /// Returns the hash of the best block (tip) of the longest chain.
    /// online zcashd reference: [`getbestblockhash`](https://zcash.github.io/rpc/getbestblockhash.html)
    /// The zcashd doc reference above says there are no parameters and the result is a "hex" (string) of the block hash hex encoded.
    /// method: post
    /// tags: blockchain
    /// The Zcash source code is considered canonical:
    /// [In the rpc definition](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/common.h#L48) there are no required params, or optional params.
    /// [The function in rpc/blockchain.cpp](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L325)
    /// where `return chainActive.Tip()->GetBlockHash().GetHex();` is the [return expression](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L339) returning a `std::string`
    fn get_best_blockhash(&self) -> impl SendFut<Result<GetBlockHash, Self::Error>>;

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// zcashd reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    fn get_raw_mempool(&self) -> impl SendFut<Result<Vec<String>, Self::Error>>;

    /// Returns information about the given block's Sapling & Orchard tree state.
    ///
    /// zcashd reference: [`z_gettreestate`](https://zcash.github.io/rpc/z_gettreestate.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash | height`: (string, required, example="00000000febc373a1da2bd9f887b105ad79ddc26ac26c2b28652d64e5207c5b5") The block hash or height.
    ///
    /// # Notes
    ///
    /// The zcashd doc reference above says that the parameter "`height` can be
    /// negative where -1 is the last known valid block". On the other hand,
    /// `lightwalletd` only uses positive heights, so Zebra does not support
    /// negative heights.
    fn z_get_treestate(
        &self,
        hash_or_height: String,
    ) -> impl SendFut<Result<GetTreestateResponse, Self::Error>>;

    /// Returns information about a range of Sapling or Orchard subtrees.
    ///
    /// zcashd reference: [`z_getsubtreesbyindex`](https://zcash.github.io/rpc/z_getsubtreesbyindex.html) - TODO: fix link
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `pool`: (string, required) The pool from which subtrees should be returned. Either "sapling" or "orchard".
    /// - `start_index`: (number, required) The index of the first 2^16-leaf subtree to return.
    /// - `limit`: (number, optional) The maximum number of subtree values to return.
    ///
    /// # Notes
    ///
    /// While Zebra is doing its initial subtree index rebuild, subtrees will become available
    /// starting at the chain tip. This RPC will return an empty list if the `start_index` subtree
    /// exists, but has not been rebuilt yet. This matches `zcashd`'s behaviour when subtrees aren't
    /// available yet. (But `zcashd` does its rebuild before syncing any blocks.)
    fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> impl SendFut<Result<GetSubtreesByIndexResponse, Self::Error>>;

    /// Returns the raw transaction data, as a [`GetRawTransaction`] JSON string or structure.
    ///
    /// zcashd reference: [`getrawtransaction`](https://zcash.github.io/rpc/getrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `txid`: (string, required, example="mytxid") The transaction ID of the transaction to be returned.
    /// - `verbose`: (number, optional, default=0, example=1) If 0, return a string of hex-encoded data, otherwise return a JSON object.
    ///
    /// # Notes
    ///
    /// We don't currently support the `blockhash` parameter since lightwalletd does not
    /// use it.
    ///
    /// In verbose mode, we only expose the `hex` and `height` fields since
    /// lightwalletd uses only those:
    /// <https://github.com/zcash/lightwalletd/blob/631bb16404e3d8b045e74a7c5489db626790b2f6/common/common.go#L119>
    fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> impl SendFut<Result<GetRawTransaction, Self::Error>>;

    /// Returns details about an unspent transaction output.
    ///
    /// zcashd reference: [`gettxout`](https://zcash.github.io/rpc/gettxout.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `txid`: (string, required) The transaction ID that contains the output.
    /// - `n`: (number, required) The output index number.
    /// - `include_mempool`: (bool, optional, default=true) Whether to include the mempool in the search.
    fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> impl SendFut<Result<zaino_fetch::jsonrpsee::response::GetTxOutResponse, Self::Error>>;

    /// Returns the txid, input index, and block height where an output is spent.
    ///
    /// zcashd reference: [`getspentinfo`](https://zcash.github.io/rpc/getspentinfo.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `request`: (object, required) with `txid` and `index`.
    ///
    /// # Notes
    ///
    /// zcashd 6.12.2 returns an undocumented `height` field in addition to
    /// the documented `txid` and `index` fields.
    fn get_spent_info(
        &self,
        request: GetSpentInfoRequest,
    ) -> impl SendFut<Result<GetSpentInfoResponse, Self::Error>>;

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
    fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> impl SendFut<Result<Vec<String>, Self::Error>>;

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
    fn z_get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> impl SendFut<Result<Vec<GetAddressUtxos>, Self::Error>>;

    /// Returns a json object containing mining-related information.
    ///
    /// `zcashd` reference (may be outdated): [`getmininginfo`](https://zcash.github.io/rpc/getmininginfo.html)
    fn get_mining_info(&self) -> impl SendFut<Result<GetMiningInfoWire, Self::Error>>;

    /// Returns statistics about the unspent transaction output set.
    ///
    /// zcashd reference: [`gettxoutsetinfo`](https://zcash.github.io/rpc/gettxoutsetinfo.html)
    /// method: post
    /// tags: blockchain
    fn get_tx_out_set_info(&self) -> impl SendFut<Result<GetTxOutSetInfoResponse, Self::Error>>;

    /// Returns the estimated network solutions per second based on the last n blocks.
    ///
    /// zcashd reference: [`getnetworksolps`](https://zcash.github.io/rpc/getnetworksolps.html)
    /// method: post
    /// tags: blockchain
    ///
    /// This RPC is implemented in the [mining.cpp](https://github.com/zcash/zcash/blob/d00fc6f4365048339c83f463874e4d6c240b63af/src/rpc/mining.cpp#L104)
    /// file of the Zcash repository. The Zebra implementation can be found [here](https://github.com/ZcashFoundation/zebra/blob/19bca3f1159f9cb9344c9944f7e1cb8d6a82a07f/zebra-rpc/src/methods.rs#L2687).
    ///
    /// # Parameters
    ///
    /// - `blocks`: (number, optional, default=120) Number of blocks, or -1 for blocks over difficulty averaging window.
    /// - `height`: (number, optional, default=-1) To estimate network speed at the time of a specific block height.
    fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> impl SendFut<Result<GetNetworkSolPsResponse, Self::Error>>;

    /// Helper function to get the chain height
    fn chain_height(&self) -> impl SendFut<Result<Height, Self::Error>>;

    /// Helper function, to get the list of taddresses that have sends or reciepts
    /// within a given block range
    fn get_taddress_txids_helper(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> impl SendFut<Result<Vec<String>, Self::Error>> {
        async move {
            let chain_height = self.chain_height().await?;
            let (start, end) = match request.range {
                Some(range) => {
                    let start = if let Some(start) = range.start {
                        match u32::try_from(start.height) {
                            Ok(height) => Some(height.min(chain_height.0)),
                            Err(_) => {
                                return Err(Self::Error::from(tonic::Status::invalid_argument(
                                    "Error: Start height out of range. Failed to convert to u32.",
                                )))
                            }
                        }
                    } else {
                        None
                    };
                    let end = if let Some(end) = range.end {
                        match u32::try_from(end.height) {
                            Ok(height) => Some(height.min(chain_height.0)),
                            Err(_) => {
                                return Err(Self::Error::from(tonic::Status::invalid_argument(
                                    "Error: End height out of range. Failed to convert to u32.",
                                )))
                            }
                        }
                    } else {
                        None
                    };
                    match (start, end) {
                        (Some(start), Some(end)) => {
                            if start > end {
                                (Some(end), Some(start))
                            } else {
                                (Some(start), Some(end))
                            }
                        }
                        _ => (start, end),
                    }
                }
                None => {
                    return Err(Self::Error::from(tonic::Status::invalid_argument(
                        "Error: No block range given.",
                    )))
                }
            };
            self.get_address_tx_ids(GetAddressTxIdsRequest::new(
                vec![request.address],
                start,
                end,
            ))
            .await
        }
    }
}

/// Light Client Protocol gRPC method signatures.
/// For more information, see [the lightwallet protocol](https://github.com/zcash/lightwallet-protocol/blob/180717dfa21f3cbf063b8a1ad7697ccba7f5b054/walletrpc/service.proto#L181).
///
/// Doc comments taken from Zaino-Proto for consistency.
pub trait LightWalletIndexer: Send + Sync + Clone + ZcashIndexer + 'static {
    /// Return the height of the tip of the best chain
    fn get_latest_block(&self) -> impl SendFut<Result<BlockId, Self::Error>>;

    /// Return the compact block corresponding to the given block identifier
    fn get_block(&self, request: BlockId) -> impl SendFut<Result<CompactBlock, Self::Error>>;

    /// Same as GetBlock except actions contain only nullifiers
    fn get_block_nullifiers(
        &self,
        request: BlockId,
    ) -> impl SendFut<Result<CompactBlock, Self::Error>>;

    /// Return a list of consecutive compact blocks
    fn get_block_range(
        &self,
        request: BlockRange,
    ) -> impl SendFut<Result<CompactBlockStream, Self::Error>>;

    /// Same as GetBlockRange except actions contain only nullifiers
    fn get_block_range_nullifiers(
        &self,
        request: BlockRange,
    ) -> impl SendFut<Result<CompactBlockStream, Self::Error>>;

    /// Return the requested full (not compact) transaction (as from zcashd)
    fn get_transaction(
        &self,
        request: TxFilter,
    ) -> impl SendFut<Result<RawTransaction, Self::Error>>;

    /// Submit the given transaction to the Zcash network
    fn send_transaction(
        &self,
        request: RawTransaction,
    ) -> impl SendFut<Result<SendResponse, Self::Error>>;

    /// Return the transactions corresponding to the given t-address within the given block range
    fn get_taddress_transactions(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> impl SendFut<Result<RawTransactionStream, Self::Error>>;

    /// Return the txids corresponding to the given t-address within the given block range
    /// Note: This function is misnamed, it returns complete `RawTransaction` values, not TxIds.
    /// Note: this method is deprecated, please use GetTaddressTransactions instead.
    fn get_taddress_txids(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> impl SendFut<Result<RawTransactionStream, Self::Error>>;

    /// Returns the total balance for a list of taddrs
    fn get_taddress_balance(
        &self,
        request: AddressList,
    ) -> impl SendFut<Result<Balance, Self::Error>>;

    /// Returns the total balance for a list of taddrs
    ///
    /// TODO: Update input type.
    fn get_taddress_balance_stream(
        &self,
        request: AddressStream,
    ) -> impl SendFut<Result<Balance, Self::Error>>;

    /// Returns a stream of the compact transaction representation for transactions
    /// currently in the mempool. The results of this operation may be a few
    /// seconds out of date. If the `exclude_txid_suffixes` list is empty,
    /// return all transactions; otherwise return all *except* those in the
    /// `exclude_txid_suffixes` list (if any); this allows the client to avoid
    /// receiving transactions that it already has (from an earlier call to this
    /// RPC). The transaction IDs in the `exclude_txid_suffixes` list can be
    /// shortened to any number of bytes to make the request more
    /// bandwidth-efficient; if two or more transactions in the mempool match a
    /// txid suffix, none of the matching transactions are excluded. Txid
    /// suffixes in the exclude list that don't match any transactions in the
    /// mempool are ignored.
    fn get_mempool_tx(
        &self,
        request: GetMempoolTxRequest,
    ) -> impl SendFut<Result<CompactTransactionStream, Self::Error>>;

    /// Return a stream of current Mempool transactions. This will keep the output stream open while
    /// there are mempool transactions. It will close the returned stream when a new block is mined.
    fn get_mempool_stream(&self) -> impl SendFut<Result<RawTransactionStream, Self::Error>>;

    /// GetTreeState returns the note commitment tree state corresponding to the given block.
    /// See section 3.7 of the Zcash protocol specification. It returns several other useful
    /// values also (even though they can be obtained using GetBlock).
    /// The block can be specified by either height or hash.
    fn get_tree_state(&self, request: BlockId) -> impl SendFut<Result<TreeState, Self::Error>>;

    /// GetLatestTreeState returns the note commitment tree state corresponding to the chain tip.
    fn get_latest_tree_state(&self) -> impl SendFut<Result<TreeState, Self::Error>>;

    /// Helper function to get timeout and channel size from config
    fn timeout_channel_size(&self) -> (u32, u32);

    /// Returns a stream of information about roots of subtrees of the Sapling and Orchard
    /// note commitment trees.
    fn get_subtree_roots(
        &self,
        request: GetSubtreeRootsArg,
    ) -> impl SendFut<Result<SubtreeRootReplyStream, <Self as ZcashIndexer>::Error>> {
        async move {
            let pool = match ShieldedProtocol::try_from(request.shielded_protocol) {
                Ok(protocol) => protocol.as_str_name(),
                Err(_) => {
                    return Err(<Self as ZcashIndexer>::Error::from(
                        tonic::Status::invalid_argument("Error: Invalid shielded protocol value."),
                    ))
                }
            };
            let start_index = match u16::try_from(request.start_index) {
                Ok(value) => value,
                Err(_) => {
                    return Err(<Self as ZcashIndexer>::Error::from(
                        tonic::Status::invalid_argument(
                            "Error: start_index value exceeds u16 range.",
                        ),
                    ))
                }
            };
            let limit = if request.max_entries == 0 {
                None
            } else {
                match u16::try_from(request.max_entries) {
                    Ok(value) => Some(value),
                    Err(_) => {
                        return Err(<Self as ZcashIndexer>::Error::from(
                            tonic::Status::invalid_argument(
                                "Error: max_entries value exceeds u16 range.",
                            ),
                        ))
                    }
                }
            };
            let service_clone = self.clone();
            let subtrees = service_clone
                .z_get_subtrees_by_index(
                    pool.to_string(),
                    NoteCommitmentSubtreeIndex(start_index),
                    limit.map(NoteCommitmentSubtreeIndex),
                )
                .await?;
            let (service_timeout, service_channel_size) = self.timeout_channel_size();
            let (channel_tx, channel_rx) = mpsc::channel(service_channel_size as usize);
            tokio::spawn(async move {
                let timeout = timeout(
                std::time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    for subtree in subtrees.subtrees() {
                        match service_clone
                            .z_get_block(subtree.end_height.0.to_string(), Some(1))
                            .await
                        {
                            Ok(GetBlock::Object(block_object)) => {
                                let checked_height = match block_object.height() {
                                    Some(h) => h.0 as u64,
                                    None => {
                                        match channel_tx
                                            .send(Err(tonic::Status::unknown(
                                                "Error: No block height returned by node.",
                                            )))
                                            .await
                                        {
                                            Ok(_) => break,
                                            Err(e) => {
                                                warn!(
                                                    %e,
                                                    "GetSubtreeRoots channel closed unexpectedly"
                                                );
                                                break;
                                            }
                                        }
                                    }
                                };
                                let checked_root_hash = match hex::decode(&subtree.root) {
                                    Ok(hash) => hash,
                                    Err(e) => {
                                        match channel_tx
                                            .send(Err(tonic::Status::unknown(format!(
                                                "Error: Failed to hex decode root hash: {e}."
                                            ))))
                                            .await
                                        {
                                            Ok(_) => break,
                                            Err(e) => {
                                                warn!(
                                                    %e,
                                                    "GetSubtreeRoots channel closed unexpectedly"
                                                );
                                                break;
                                            }
                                        }
                                    }
                                };
                                if channel_tx
                                    .send(Ok(SubtreeRoot {
                                        root_hash: checked_root_hash,
                                        completing_block_hash: block_object
                                            .hash()
                                            .bytes_in_display_order()
                                            .to_vec(),
                                        completing_block_height: checked_height,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Ok(GetBlock::Raw(_)) => {
                                // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                                if channel_tx
                                .send(Err(tonic::Status::unknown(
                                    "Error: Received raw block type, this should not be possible.",
                                )))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            }
                            Err(e) => {
                                // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                                if channel_tx
                                    .send(Err(tonic::Status::unknown(format!(
                                        "Error: Could not fetch block at height [{}] from node: {}",
                                        subtree.end_height.0, e
                                    ))))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                },
            )
            .await;
                match timeout {
                    Ok(_) => {}
                    Err(_) => {
                        channel_tx
                            .send(Err(tonic::Status::deadline_exceeded(
                                "Error: get_mempool_stream gRPC request timed out",
                            )))
                            .await
                            .ok();
                    }
                }
            });
            Ok(SubtreeRootReplyStream::new(channel_rx))
        }
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// max_entries bounds the response size, not the backend work; the address list is
    /// capped server-side to bound backend fan-out (see UTXO_MAX_ADDRESSES in backends).
    /// Utxos are collected and returned as a single Vec.
    fn get_address_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> impl SendFut<Result<GetAddressUtxosReplyList, Self::Error>>;

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// max_entries bounds the response size, not the backend work; the address list is
    /// capped server-side to bound backend fan-out (see UTXO_MAX_ADDRESSES in backends).
    /// Utxos are returned in a stream.
    fn get_address_utxos_stream(
        &self,
        request: GetAddressUtxosArg,
    ) -> impl SendFut<Result<UtxoReplyStream, Self::Error>>;

    /// Return information about this lightwalletd instance and the blockchain
    fn get_lightd_info(&self) -> impl SendFut<Result<LightdInfo, Self::Error>>;

    /// Testing-only, requires lightwalletd --ping-very-insecure (do not enable in production)
    ///
    /// NOTE: Currently unimplemented in Zaino.
    fn ping(&self, request: Duration) -> impl SendFut<Result<PingResponse, Self::Error>>;
}

/// Zcash Service functionality.
pub trait LightWalletService: Sized + ZcashService<Subscriber: LightWalletIndexer> {}

impl<T> LightWalletService for T where T: ZcashService {}

pub(crate) async fn handle_raw_transaction<Indexer: LightWalletIndexer>(
    chain_height: u64,
    transaction: Result<GetRawTransaction, Indexer::Error>,
    transmitter: mpsc::Sender<Result<RawTransaction, tonic::Status>>,
) -> Result<(), mpsc::error::SendError<Result<RawTransaction, tonic::Status>>> {
    match transaction {
        Ok(GetRawTransaction::Object(transaction_obj)) => {
            let height: u64 = match transaction_obj.height() {
                Some(h) => h as u64,
                // Zebra returns None for mempool transactions, convert to `Mempool Height`.
                None => chain_height,
            };
            transmitter
                .send(Ok(RawTransaction {
                    data: transaction_obj.hex().as_ref().to_vec(),
                    height,
                }))
                .await
        }
        Ok(GetRawTransaction::Raw(_)) => {
            transmitter
                .send(Err(tonic::Status::unknown(
                    "Received raw transaction type, this should not be impossible.",
                )))
                .await
        }
        Err(e) => {
            // TODO: Hide server error from clients before release. Currently useful for dev purposes.
            transmitter
                .send(Err(tonic::Status::unknown(e.to_string())))
                .await
        }
    }
}

/// Maps a Zebra network to the `zcash_protocol` network type used for address decoding.
fn address_network_type(
    network: &zebra_chain::parameters::Network,
) -> zcash_protocol::consensus::NetworkType {
    use zcash_protocol::consensus::NetworkType;
    use zebra_chain::parameters::NetworkKind;
    match network.kind() {
        NetworkKind::Mainnet => NetworkType::Main,
        NetworkKind::Testnet => NetworkType::Test,
        NetworkKind::Regtest => NetworkType::Regtest,
    }
}

/// Validates a Zcash address for the `validateaddress` RPC.
///
/// Pure address parsing over `network`; no chain data required, so both backends share
/// this implementation.
pub(crate) fn validate_address(
    raw_address: String,
    network: &zebra_chain::parameters::Network,
) -> ValidateAddressResponse {
    use zcash_keys::address::Address;
    use zcash_transparent::address::TransparentAddress;

    let Ok(address) = raw_address.parse::<zcash_address::ZcashAddress>() else {
        return ValidateAddressResponse::invalid();
    };

    let address = match address.convert_if_network::<Address>(address_network_type(network)) {
        Ok(address) => address,
        Err(err) => {
            tracing::debug!(?err, "conversion error");
            return ValidateAddressResponse::invalid();
        }
    };

    match address {
        Address::Transparent(taddr) => ValidateAddressResponse::new(
            true,
            Some(raw_address),
            Some(matches!(taddr, TransparentAddress::ScriptHash(_))),
        ),
        _ => ValidateAddressResponse::invalid(),
    }
}

/// Validates a Zcash address for the deprecated `z_validateaddress` RPC.
///
/// Pure address parsing over `network`; shared by both backends.
pub(crate) fn z_validate_address(
    address: String,
    network: &zebra_chain::parameters::Network,
) -> ZValidateAddressResponse {
    use zcash_keys::address::Address;
    use zcash_keys::encoding::AddressCodec as _;
    use zcash_transparent::address::TransparentAddress;

    tracing::warn!("{}", Z_VALIDATE_DEPRECATION);

    let invalid = || {
        ZValidateAddressResponse::Known(KnownZValidateAddress::Invalid(
            InvalidZValidateAddress::new(),
        ))
    };

    let Ok(parsed_address) = address.parse::<zcash_address::ZcashAddress>() else {
        return invalid();
    };

    let converted_address =
        match parsed_address.convert_if_network::<Address>(address_network_type(network)) {
            Ok(address) => address,
            Err(err) => {
                tracing::debug!(?err, "conversion error");
                return invalid();
            }
        };

    // Note: It could be the case that Zaino needs to support Sprout. For now, it's been disabled.
    match converted_address {
        Address::Transparent(TransparentAddress::PublicKeyHash(_)) => {
            ZValidateAddressResponse::p2pkh(address)
        }
        Address::Transparent(TransparentAddress::ScriptHash(_)) => {
            ZValidateAddressResponse::p2sh(address)
        }
        Address::Unified(u) => ZValidateAddressResponse::unified(u.encode(network)),
        Address::Sapling(s) => {
            let (diversifier, pk_d) = sapling_key_bytes(&s);
            ZValidateAddressResponse::sapling(
                s.encode(network),
                Some(hex::encode(diversifier)),
                Some(hex::encode(pk_d)),
            )
        }
        _ => invalid(),
    }
}

/// Extracts the diversifier and pk_d bytes from a validated Sapling
/// [`sapling_crypto::PaymentAddress`], returning pk_d in zcashd's big-endian byte order.
///
/// # Deprecation
///
/// See [`Z_VALIDATE_DEPRECATION`]. This function exists to support the `z_validateaddress`
/// RPC endpoint, which itself exists solely for zcashd compatibility. The pk_d bytes are
/// reversed from `sapling-crypto`'s native little-endian representation to match zcashd's
/// big-endian hex output.
///
/// # Precondition
///
/// The caller must have obtained `s` through `PaymentAddress::from_bytes` or equivalent
/// (e.g. `ZcashAddress::convert_if_network`), which guarantees the diversifier has a valid
/// `g_d()` and pk_d is a non-identity Jubjub subgroup point. No additional validation is
/// performed here.
///
/// # Layout
///
/// `PaymentAddress::to_bytes()` returns 43 bytes: `diversifier (11) || pk_d (32)`.
pub(crate) fn sapling_key_bytes(s: &sapling_crypto::PaymentAddress) -> ([u8; 11], [u8; 32]) {
    let bytes = s.to_bytes();
    let diversifier: [u8; 11] = bytes[..11]
        .try_into()
        .expect("PaymentAddress::to_bytes always returns 43 bytes: diversifier is the first 11");
    let mut pk_d: [u8; 32] = bytes[11..]
        .try_into()
        .expect("PaymentAddress::to_bytes always returns 43 bytes: pk_d is the last 32");
    pk_d.reverse();
    (diversifier, pk_d)
}

/// Shapes a `z_getsubtreesbyindex` JSON-RPC response from raw subtree roots.
///
/// The `(root, end_height)` pairs from [`ChainIndex::get_subtree_roots`] are already in
/// the byte order the JSON-RPC uses (sapling `to_bytes`, orchard `to_repr` — zcashd's
/// `z_getsubtreesbyindex` does not reverse orchard subtree roots), so they are hex-encoded
/// as-is. Shared by both backends so the shaping lives in one place.
///
/// [`ChainIndex::get_subtree_roots`]: crate::ChainIndex::get_subtree_roots
pub(crate) fn build_subtrees_by_index_response(
    pool: String,
    start_index: NoteCommitmentSubtreeIndex,
    roots: Vec<([u8; 32], u32)>,
) -> GetSubtreesByIndexResponse {
    use hex::ToHex as _;

    let subtrees = roots
        .into_iter()
        .map(|(root, end_height)| {
            SubtreeRpcData {
                root: root.encode_hex(),
                end_height: Height(end_height),
            }
            .into()
        })
        .collect();

    GetSubtreesResponse {
        pool,
        start_index,
        subtrees,
    }
    .into()
}

/// Builds the gRPC [`TreeState`](zaino_proto::proto::service::TreeState) from a
/// `z_gettreestate` response: hex-encoded per-pool final states (the ironwood field is
/// the empty string below NU6.3 activation, matching lightwalletd behaviour).
fn tree_state_from_treestate_response(
    network: String,
    treestate_response: zebra_rpc::client::GetTreestateResponse,
) -> zaino_proto::proto::service::TreeState {
    let sapling_tree = hex::encode(
        treestate_response
            .sapling()
            .commitments()
            .final_state()
            .clone()
            .unwrap_or_default(),
    );
    let orchard_tree = hex::encode(
        treestate_response
            .orchard()
            .commitments()
            .final_state()
            .clone()
            .unwrap_or_default(),
    );
    let ironwood_tree = treestate_response
        .ironwood()
        .clone()
        .and_then(|treestate| treestate.commitments().final_state().clone())
        .map(hex::encode)
        .unwrap_or_default();

    zaino_proto::proto::service::TreeState {
        network,
        height: treestate_response.height().0 as u64,
        hash: treestate_response.hash().to_string(),
        time: treestate_response.time(),
        sapling_tree,
        orchard_tree,
        ironwood_tree,
    }
}

/// Builds the `z_gettreestate` response from the per-pool treestates the chain index
/// reported.
///
/// `Commitments::new(final_root, final_state)`: the note-commitment tree is the
/// `final_state`. The ironwood treestate is `Some` only from NU6.3 activation, so
/// pre-NU6.3 responses omit the field exactly as zebrad does.
fn build_treestate_response(
    hash: zebra_chain::block::Hash,
    height: zebra_chain::block::Height,
    time: u32,
    (sapling, orchard, ironwood): (
        Option<crate::chain_index::source::PoolTreestate>,
        Option<crate::chain_index::source::PoolTreestate>,
        Option<crate::chain_index::source::PoolTreestate>,
    ),
) -> zebra_rpc::client::GetTreestateResponse {
    fn treestate(
        pool: Option<crate::chain_index::source::PoolTreestate>,
    ) -> zebra_rpc::client::Treestate {
        let (final_root, final_state) = match pool {
            Some(pool) => (pool.final_root, Some(pool.final_state)),
            None => (None, None),
        };
        zebra_rpc::client::Treestate::new(zebra_rpc::client::Commitments::new(
            final_root,
            final_state,
        ))
    }

    let sprout_treestate = None;
    let ironwood_treestate = ironwood.map(|pool| treestate(Some(pool)));
    zebra_rpc::client::GetTreestateResponse::new(
        hash,
        height,
        time,
        sprout_treestate,
        treestate(sapling),
        treestate(orchard),
        ironwood_treestate,
    )
}

fn latest_network_upgrade(
    upgrades: &indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    >,
) -> Result<&zebra_rpc::methods::NetworkUpgradeInfo, tonic::Status> {
    upgrades.last().map(|(_, upgrade)| upgrade).ok_or_else(|| {
        tonic::Status::failed_precondition("validator returned no network upgrade metadata")
    })
}

/// Maximum number of addresses a single `get_address_utxos` / `get_address_utxos_stream`
/// request may carry.
///
/// The service resolves the full backend UTXO set before applying `max_entries` /
/// `start_height` (issue #974). A complete pushdown fix needs upstream interface changes
/// the caller-supplied entry cap cannot reach today, so until then this bounds the one
/// input the service controls locally: the address fan-out. It stops an unauthenticated
/// caller forcing an unbounded number of backend address lookups in a single request, and
/// is set well above realistic wallet usage.
///
/// TODO: make this deployment-configurable rather than a fixed constant.
const UTXO_MAX_ADDRESSES: usize = 1000;

/// Reject a `get_address_utxos` request whose address list exceeds [`UTXO_MAX_ADDRESSES`].
///
/// `max_entries` bounds the response size, not the backend work; this guard bounds the
/// address fan-out, the part the service can cap without upstream changes.
fn validate_utxo_address_count(count: usize) -> Result<(), tonic::Status> {
    if count > UTXO_MAX_ADDRESSES {
        return Err(tonic::Status::invalid_argument(format!(
            "Error: too many addresses in request: {count} exceeds the maximum of {UTXO_MAX_ADDRESSES}."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_network_upgrade_rejects_empty_metadata() {
        let upgrades = indexmap::IndexMap::new();
        let err = super::latest_network_upgrade(&upgrades).expect_err("empty upgrades must fail");

        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            err.message(),
            "validator returned no network upgrade metadata"
        );
    }

    #[test]
    fn utxo_address_count_within_limit_is_accepted() {
        assert!(super::validate_utxo_address_count(0).is_ok());
        assert!(super::validate_utxo_address_count(super::UTXO_MAX_ADDRESSES).is_ok());
    }

    #[test]
    fn utxo_address_count_over_limit_is_rejected() {
        let err = super::validate_utxo_address_count(super::UTXO_MAX_ADDRESSES + 1)
            .expect_err("over-limit address count must fail");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn build_subtrees_by_index_response_hex_encodes_roots() {
        let roots = vec![([0xabu8; 32], 100u32), ([0xcdu8; 32], 200u32)];
        let response = build_subtrees_by_index_response(
            "orchard".to_string(),
            NoteCommitmentSubtreeIndex(5),
            roots,
        );

        assert_eq!(response.pool().as_str(), "orchard");
        assert_eq!(response.start_index(), NoteCommitmentSubtreeIndex(5));

        let subtrees = response.subtrees();
        assert_eq!(subtrees.len(), 2);
        assert_eq!(subtrees[0].root, hex::encode([0xabu8; 32]));
        assert_eq!(subtrees[0].end_height, Height(100));
        assert_eq!(subtrees[1].root, hex::encode([0xcdu8; 32]));
        assert_eq!(subtrees[1].end_height, Height(200));
    }

    /// Classifies the byte-level relationship between two slices.
    #[derive(Debug, PartialEq)]
    enum ByteRelation {
        /// The slices are identical.
        Equal,
        /// `actual` fully byte-reversed equals `expected` (endian swap).
        FullByteReversal,
        /// Each byte's bits reversed maps `actual` to `expected`.
        PerByteBitReversal,
        /// Reversing bytes within 16-bit chunks maps `actual` to `expected`.
        ChunkSwap16,
        /// Reversing bytes within 32-bit chunks maps `actual` to `expected`.
        ChunkSwap32,
        /// Reversing bytes within 64-bit chunks maps `actual` to `expected`.
        ChunkSwap64,
        /// No recognized transformation.
        Unrecognized,
    }

    impl std::fmt::Display for ByteRelation {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Equal => write!(f, "equal"),
                Self::FullByteReversal => write!(f, "full byte-reversal (endian swap)"),
                Self::PerByteBitReversal => write!(f, "per-byte bit-reversal"),
                Self::ChunkSwap16 => write!(f, "16-bit pairwise byte-swap"),
                Self::ChunkSwap32 => write!(f, "32-bit chunk byte-reversal"),
                Self::ChunkSwap64 => write!(f, "64-bit chunk byte-reversal"),
                Self::Unrecognized => write!(f, "unrecognized mismatch"),
            }
        }
    }

    /// Applies each candidate byte transformation to `actual` and returns
    /// the first that produces `expected`, or [`ByteRelation::Unrecognized`].
    // `u32::is_multiple_of` is only stable from Rust 1.87; keep `% n == 0` for our older MSRV.
    #[allow(clippy::manual_is_multiple_of)]
    fn classify_byte_relation(actual: &[u8], expected: &[u8]) -> ByteRelation {
        if actual == expected {
            return ByteRelation::Equal;
        }

        let chunk_swap = |size: usize| -> Vec<u8> {
            actual
                .chunks(size)
                .flat_map(|c| c.iter().rev())
                .copied()
                .collect()
        };

        let mut reversed = actual.to_vec();
        reversed.reverse();
        if reversed == expected {
            return ByteRelation::FullByteReversal;
        }

        let bit_reversed: Vec<u8> = actual.iter().map(|b| b.reverse_bits()).collect();
        if bit_reversed == expected {
            return ByteRelation::PerByteBitReversal;
        }

        if actual.len() % 2 == 0 && chunk_swap(2) == expected {
            return ByteRelation::ChunkSwap16;
        }
        if actual.len() % 4 == 0 && chunk_swap(4) == expected {
            return ByteRelation::ChunkSwap32;
        }
        if actual.len() % 8 == 0 && chunk_swap(8) == expected {
            return ByteRelation::ChunkSwap64;
        }

        ByteRelation::Unrecognized
    }

    /// Verifies that our Sapling address parsing logic produces the same
    /// diversifier and diversified transmission key (pk_d) hex strings as
    /// zcashd's `z_validateaddress` RPC.
    ///
    /// # Guarantees
    ///
    /// - Exercises the production `sapling_key_bytes` function directly.
    /// - The 11-byte diversifier matches the zcashd-derived test vector.
    /// - The 32-byte pk_d (after the endian reversal inside `sapling_key_bytes`)
    ///   matches the zcashd-derived test vector.
    /// - If the upstream serialization changes, the failure message
    ///   classifies the mismatch (endian swap, bit-reversal, chunk swap,
    ///   or unrecognized) to aid diagnosis.
    ///
    /// # Non-guarantees
    ///
    /// - Does not prove the test vector constants themselves are correct;
    ///   they were captured from zcashd and are trusted as ground truth.
    /// - Does not exercise the full `z_validate_address` RPC path through
    ///   `StateService` — only the `sapling_key_bytes` extraction function.
    /// - Does not verify behavior for malformed Sapling addresses or
    ///   addresses on other networks (mainnet, testnet).
    #[test]
    fn sapling_pk_d_byte_order_matches_test_vector() {
        use crate::indexer::sapling_key_bytes;
        use zcash_keys::address::Address;
        use zcash_protocol::consensus::NetworkType;

        // Canonical source: live-tests/clientless/src/lib.rs::rpc::json_rpc
        // Tracked for DRY consolidation: https://github.com/zingolabs/zaino/issues/988
        const SAPLING_ADDRESS: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
        const EXPECTED_DIVERSIFIER: &str = "977e0b930ee6c11e4d26f8";
        const EXPECTED_PK_D: &str =
            "553ef2f328096a7c2aac6dec85b76b6b9243e733dc9db2eacce3eb8c60592c88";

        let parsed: zcash_address::ZcashAddress = SAPLING_ADDRESS.parse().unwrap();
        let converted = parsed
            .convert_if_network::<Address>(NetworkType::Regtest)
            .unwrap();

        let Address::Sapling(s) = converted else {
            panic!("expected Sapling address");
        };

        let (diversifier, pk_d) = sapling_key_bytes(&s);

        let expected_diversifier = hex::decode(EXPECTED_DIVERSIFIER).unwrap();
        let expected_pk_d = hex::decode(EXPECTED_PK_D).unwrap();

        // Diversifier
        match classify_byte_relation(&diversifier, &expected_diversifier) {
            ByteRelation::Equal => {}
            relation => panic!(
                "diversifier mismatch.\n  relation: {relation}\n  actual:   {}\n  expected: {}",
                hex::encode(diversifier),
                hex::encode(expected_diversifier),
            ),
        }

        // pk_d (sapling_key_bytes already applies the endian reversal)
        match classify_byte_relation(&pk_d, &expected_pk_d) {
            ByteRelation::Equal => {}
            relation => panic!(
                "pk_d mismatch — upstream serialization may have changed.\
                \n  relation: {relation}\n  actual:   {}\n  expected: {}",
                hex::encode(pk_d),
                hex::encode(expected_pk_d),
            ),
        }
    }

    #[test]
    fn classify_byte_relation_detects_known_transforms() {
        let original = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];

        assert_eq!(
            classify_byte_relation(&original, &original),
            ByteRelation::Equal,
        );

        let mut reversed = original.to_vec();
        reversed.reverse();
        assert_eq!(
            classify_byte_relation(&original, &reversed),
            ByteRelation::FullByteReversal,
        );

        let bit_rev: Vec<u8> = original.iter().map(|b| b.reverse_bits()).collect();
        assert_eq!(
            classify_byte_relation(&original, &bit_rev),
            ByteRelation::PerByteBitReversal,
        );

        let swapped_16: Vec<u8> = original
            .chunks(2)
            .flat_map(|c| c.iter().rev())
            .copied()
            .collect();
        assert_eq!(
            classify_byte_relation(&original, &swapped_16),
            ByteRelation::ChunkSwap16,
        );

        let garbage = [0xFF; 8];
        assert_eq!(
            classify_byte_relation(&original, &garbage),
            ByteRelation::Unrecognized,
        );
    }
}
