//! Zcash chain fetch and tx submission service backed by zcashds JsonRPC service.

use futures::StreamExt;
use hex::FromHex;
use std::time;
use tokio::{sync::mpsc, time::timeout};
use tonic::async_trait;
use tracing::{info, warn};
use zebra_state::HashOrHeight;

use zebra_chain::{block::Height, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::{
    client::{GetSubtreesByIndexResponse, GetTreestateResponse, ValidateAddressResponse},
    methods::{
        AddressBalance, AddressStrings, GetAddressTxIdsRequest, GetAddressUtxos, GetBlock,
        GetBlockHashResponse, GetBlockchainInfoResponse, GetInfo, GetRawTransaction,
        SentTransactionHash,
    },
};

use zaino_fetch::{
    chain::{transaction::FullTransaction, utils::ParseFromSlice},
    jsonrpsee::{
        connector::{JsonRpSeeConnector, RpcError},
        response::{
            address_deltas::{GetAddressDeltasParams, GetAddressDeltasResponse},
            block_deltas::BlockDeltas,
            block_header::GetBlockHeader,
            block_subsidy::GetBlockSubsidy,
            mining_info::GetMiningInfoWire,
            peer_info::GetPeerInfo,
            GetMempoolInfoResponse, GetNetworkSolPsResponse,
        },
    },
};

use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, Balance, BlockId, BlockRange, Duration, Exclude, GetAddressUtxosArg,
        GetAddressUtxosReply, GetAddressUtxosReplyList, LightdInfo, PingResponse, RawTransaction,
        SendResponse, TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};

#[allow(deprecated)]
use crate::{
    chain_index::{
        mempool::{Mempool, MempoolSubscriber},
        source::ValidatorConnector,
    },
    config::FetchServiceConfig,
    error::{BlockCacheError, FetchServiceError},
    indexer::{
        handle_raw_transaction, IndexerSubscriber, LightWalletIndexer, ZcashIndexer, ZcashService,
    },
    local_cache::{BlockCache, BlockCacheSubscriber},
    status::StatusType,
    stream::{
        AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
        UtxoReplyStream,
    },
    utils::{blockid_to_hashorheight, get_build_info, ServiceMetadata},
    BackendType,
};

/// Chain fetch service backed by Zcashd's JsonRPC engine.
///
/// This service is a central service, [`FetchServiceSubscriber`] should be created to fetch data.
/// This is done to enable large numbers of concurrent subscribers without significant slowdowns.
///
/// NOTE: We currently do not implement clone for chain fetch services as this service is responsible for maintaining and closing its child processes.
///       ServiceSubscribers are used to create separate chain fetch processes while allowing central state processes to be managed in a single place.
///       If we want the ability to clone Service all JoinHandle's should be converted to Arc\<JoinHandle\>.
#[derive(Debug)]
#[deprecated = "Will be eventually replaced by `BlockchainSource`"]
pub struct FetchService {
    /// JsonRPC Client.
    fetcher: JsonRpSeeConnector,

    /// Local compact block cache.
    block_cache: BlockCache,

    /// Internal mempool.
    mempool: Mempool<ValidatorConnector>,

    /// Service metadata.
    data: ServiceMetadata,

    /// StateService config data.
    #[allow(deprecated)]
    config: FetchServiceConfig,
}

#[async_trait]
#[allow(deprecated)]
impl ZcashService for FetchService {
    const BACKEND_TYPE: BackendType = BackendType::Fetch;

    type Subscriber = FetchServiceSubscriber;
    type Config = FetchServiceConfig;

    /// Initializes a new FetchService instance and starts sync process.
    async fn spawn(config: FetchServiceConfig) -> Result<Self, FetchServiceError> {
        info!("Launching Chain Fetch Service..");

        let fetcher = JsonRpSeeConnector::new_from_config_parts(
            config.validator_rpc_address,
            config.validator_rpc_user.clone(),
            config.validator_rpc_password.clone(),
            config.validator_cookie_path.clone(),
        )
        .await?;

        let zebra_build_data = fetcher.get_info().await?;
        let data = ServiceMetadata::new(
            get_build_info(),
            config.network.to_zebra_network(),
            zebra_build_data.build,
            zebra_build_data.subversion,
        );
        info!("Using Zcash build: {}", data);

        let block_cache = BlockCache::spawn(&fetcher, None, config.clone().into())
            .await
            .map_err(|e| {
                FetchServiceError::BlockCacheError(BlockCacheError::Custom(e.to_string()))
            })?;

        let mempool_source = ValidatorConnector::Fetch(fetcher.clone());

        let mempool = Mempool::spawn(mempool_source, None).await.map_err(|e| {
            FetchServiceError::BlockCacheError(BlockCacheError::Custom(e.to_string()))
        })?;

        let fetch_service = Self {
            fetcher,
            block_cache,
            mempool,
            data,
            config,
        };

        while fetch_service.status().await != StatusType::Ready {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        Ok(fetch_service)
    }

    /// Returns a [`FetchServiceSubscriber`].
    fn get_subscriber(&self) -> IndexerSubscriber<FetchServiceSubscriber> {
        IndexerSubscriber::new(FetchServiceSubscriber {
            fetcher: self.fetcher.clone(),
            block_cache: self.block_cache.subscriber(),
            mempool: self.mempool.subscriber(),
            data: self.data.clone(),
            config: self.config.clone(),
        })
    }

    /// Fetches the current status
    async fn status(&self) -> StatusType {
        let mempool_status = self.mempool.status();
        let block_cache_status = self.block_cache.status();

        mempool_status.combine(block_cache_status)
    }

    /// Shuts down the StateService.
    fn close(&mut self) {
        self.mempool.close();
        self.block_cache.close();
    }
}

#[allow(deprecated)]
impl Drop for FetchService {
    fn drop(&mut self) {
        self.close()
    }
}

/// A fetch service subscriber.
///
/// Subscribers should be
#[derive(Debug, Clone)]
#[allow(deprecated)]
pub struct FetchServiceSubscriber {
    /// JsonRPC Client.
    pub fetcher: JsonRpSeeConnector,

    /// Local compact block cache.
    pub block_cache: BlockCacheSubscriber,

    /// Internal mempool.
    pub mempool: MempoolSubscriber,

    /// Service metadata.
    pub data: ServiceMetadata,

    /// StateService config data.
    #[allow(deprecated)]
    config: FetchServiceConfig,
}

impl FetchServiceSubscriber {
    /// Fetches the current status
    pub fn status(&self) -> StatusType {
        let mempool_status = self.mempool.status();
        let block_cache_status = self.block_cache.status();

        mempool_status.combine(block_cache_status)
    }

    /// Returns the network type running.
    #[allow(deprecated)]
    pub fn network(&self) -> zaino_common::Network {
        self.config.network
    }
}

#[async_trait]
#[allow(deprecated)]
impl ZcashIndexer for FetchServiceSubscriber {
    type Error = FetchServiceError;

    /// Returns information about all changes to the given transparent addresses within the given inclusive block-height range.
    ///
    /// Defaults: if start or end are not specified, they default to 0.
    ///
    /// ### Normalization
    ///
    /// - If start is greater than the latest block height (tip), start is set to the tip.
    /// - If end is 0 or greater than the tip, end is set to the tip.
    ///
    /// ### Validation
    ///
    /// If the resulting start is greater than end, the call fails with an error.
    /// (Thus, [tip, tip] is valid and returns only the tip block.)
    ///
    /// [Original zcashd implementation](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/misc.cpp#L881)
    ///
    /// zcashd reference: [`getaddressdeltas`](https://zcash.github.io/rpc/getaddressdeltas.html)
    /// method: post
    /// tags: address
    async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> Result<GetAddressDeltasResponse, Self::Error> {
        Ok(self.fetcher.get_address_deltas(params).await?)
    }

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
    async fn get_info(&self) -> Result<GetInfo, Self::Error> {
        Ok(self.fetcher.get_info().await?.into())
    }

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
    async fn get_blockchain_info(&self) -> Result<GetBlockchainInfoResponse, Self::Error> {
        Ok(self
            .fetcher
            .get_blockchain_info()
            .await?
            .try_into()
            .map_err(|_e| {
                FetchServiceError::SerializationError(
                    zebra_chain::serialization::SerializationError::Parse(
                        "chainwork not hex-encoded integer",
                    ),
                )
            })?)
    }

    /// Returns details on the active state of the TX memory pool.
    ///
    /// online zcash rpc reference: [`getmempoolinfo`](https://zcash.github.io/rpc/getmempoolinfo.html)
    /// method: post
    /// tags: mempool
    ///
    /// Canonical source code implementation: [`getmempoolinfo`](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/blockchain.cpp#L1555)
    ///
    /// Zebra does not support this RPC call directly.
    async fn get_mempool_info(&self) -> Result<GetMempoolInfoResponse, Self::Error> {
        Ok(self.mempool.get_mempool_info().await?)
    }

    async fn get_peer_info(&self) -> Result<GetPeerInfo, Self::Error> {
        Ok(self.fetcher.get_peer_info().await?)
    }

    /// Returns the proof-of-work difficulty as a multiple of the minimum difficulty.
    ///
    /// zcashd reference: [`getdifficulty`](https://zcash.github.io/rpc/getdifficulty.html)
    /// method: post
    /// tags: blockchain
    async fn get_difficulty(&self) -> Result<f64, Self::Error> {
        Ok(self.fetcher.get_difficulty().await?.0)
    }

    async fn get_block_subsidy(&self, height: u32) -> Result<GetBlockSubsidy, Self::Error> {
        Ok(self.fetcher.get_block_subsidy(height).await?)
    }

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
    async fn z_get_address_balance(
        &self,
        address_strings: AddressStrings,
    ) -> Result<AddressBalance, Self::Error> {
        Ok(self
            .fetcher
            .get_address_balance(address_strings.valid_address_strings().map_err(|error| {
                FetchServiceError::RpcError(RpcError {
                    code: error.code() as i64,
                    message: "Invalid address provided".to_string(),
                    data: None,
                })
            })?)
            .await?
            .into())
    }

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
    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<SentTransactionHash, Self::Error> {
        Ok(self
            .fetcher
            .send_raw_transaction(raw_transaction_hex)
            .await?
            .into())
    }

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
    async fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        Ok(self
            .fetcher
            .get_block(hash_or_height, verbosity)
            .await?
            .try_into()?)
    }

    /// Returns information about the given block and its transactions.
    ///
    /// zcashd reference: [`getblockdeltas`](https://zcash.github.io/rpc/getblockdeltas.html)
    /// method: post
    /// tags: blockchain
    ///
    /// Note: This method has only been implemented in `zcashd`. Zebra has no intention of supporting it.
    async fn get_block_deltas(&self, hash: String) -> Result<BlockDeltas, Self::Error> {
        Ok(self.fetcher.get_block_deltas(hash).await?)
    }

    async fn get_block_header(
        &self,
        hash: String,
        verbose: bool,
    ) -> Result<GetBlockHeader, Self::Error> {
        Ok(self.fetcher.get_block_header(hash, verbose).await?)
    }

    async fn get_mining_info(&self) -> Result<GetMiningInfoWire, Self::Error> {
        Ok(self.fetcher.get_mining_info().await?)
    }

    /// Returns the hash of the best block (tip) of the longest chain.
    /// online zcashd reference: [`getbestblockhash`](https://zcash.github.io/rpc/getbestblockhash.html)
    /// The zcashd doc reference above says there are no parameters and the result is a "hex" (string) of the block hash hex encoded.
    /// method: post
    /// tags: blockchain
    /// Return the hex encoded hash of the best (tip) block, in the longest block chain.
    ///
    /// # Parameters
    ///
    /// No request parameters.
    ///
    /// # Notes
    ///
    /// Return should be valid hex encoded.
    ///
    /// The Zcash source code is considered canonical:
    /// [In the rpc definition](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/common.h#L48) there are no required params, or optional params.
    /// [The function in rpc/blockchain.cpp](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L325)
    /// where `return chainActive.Tip()->GetBlockHash().GetHex();` is the [return expression](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L339)returning a `std::string`
    async fn get_best_blockhash(&self) -> Result<GetBlockHashResponse, Self::Error> {
        Ok(self.fetcher.get_best_blockhash().await?.into())
    }

    /// Returns the current block count in the best valid block chain.
    ///
    /// zcashd reference: [`getblockcount`](https://zcash.github.io/rpc/getblockcount.html)
    /// method: post
    /// tags: blockchain
    async fn get_block_count(&self) -> Result<Height, Self::Error> {
        Ok(self.fetcher.get_block_count().await?.into())
    }

    /// Return information about the given Zcash address.
    ///
    /// # Parameters
    /// - `address`: (string, required, example="tmHMBeeYRuc2eVicLNfP15YLxbQsooCA6jb") The Zcash transparent address to validate.
    ///
    /// zcashd reference: [`validateaddress`](https://zcash.github.io/rpc/validateaddress.html)
    /// method: post
    /// tags: blockchain
    async fn validate_address(
        &self,
        address: String,
    ) -> Result<ValidateAddressResponse, Self::Error> {
        Ok(self.fetcher.validate_address(address).await?)
    }

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// zcashd reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    async fn get_raw_mempool(&self) -> Result<Vec<String>, Self::Error> {
        // Ok(self.fetcher.get_raw_mempool().await?.transactions)
        Ok(self
            .mempool
            .get_mempool()
            .await
            .into_iter()
            .map(|(key, _)| key.txid)
            .collect())
    }

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
    async fn z_get_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<GetTreestateResponse, Self::Error> {
        Ok(self
            .fetcher
            .get_treestate(hash_or_height)
            .await?
            .try_into()?)
    }

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
    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<GetSubtreesByIndexResponse, Self::Error> {
        Ok(self
            .fetcher
            .get_subtrees_by_index(pool, start_index.0, limit.map(|limit_index| limit_index.0))
            .await?
            .into())
    }

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
    async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetRawTransaction, Self::Error> {
        Ok(self
            .fetcher
            .get_raw_transaction(txid_hex, verbose)
            .await?
            .into())
    }

    async fn chain_height(&self) -> Result<Height, Self::Error> {
        Ok(self.block_cache.get_chain_height().await?)
    }
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
    async fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, Self::Error> {
        let (addresses, start, end) = request.into_parts();
        Ok(self
            .fetcher
            .get_address_txids(addresses, start, end)
            .await?
            .transactions)
    }

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
    async fn z_get_address_utxos(
        &self,
        address_strings: AddressStrings,
    ) -> Result<Vec<GetAddressUtxos>, Self::Error> {
        Ok(self
            .fetcher
            .get_address_utxos(address_strings.valid_address_strings().map_err(|error| {
                FetchServiceError::RpcError(RpcError {
                    code: error.code() as i64,
                    message: "Invalid address provided".to_string(),
                    data: None,
                })
            })?)
            .await?
            .into_iter()
            .map(|utxos| utxos.into())
            .collect())
    }

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
    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> Result<GetNetworkSolPsResponse, Self::Error> {
        Ok(self.fetcher.get_network_sol_ps(blocks, height).await?)
    }
}

#[async_trait]
#[allow(deprecated)]
impl LightWalletIndexer for FetchServiceSubscriber {
    /// Return the height of the tip of the best chain
    async fn get_latest_block(&self) -> Result<BlockId, Self::Error> {
        let latest_height = self.block_cache.get_chain_height().await?;
        let latest_hash = self
            .block_cache
            .get_compact_block(latest_height.0.to_string())
            .await?
            .hash;

        Ok(BlockId {
            height: latest_height.0 as u64,
            hash: latest_hash,
        })
    }

    /// Return the compact block corresponding to the given block identifier
    async fn get_block(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        let hash_or_height = blockid_to_hashorheight(request).ok_or(
            FetchServiceError::TonicStatusError(tonic::Status::invalid_argument(
                "Error: Invalid hash and/or height out of range. Failed to convert to u32.",
            )),
        )?;
        match self
            .block_cache
            .get_compact_block(hash_or_height.to_string())
            .await
        {
            Ok(block) => Ok(block),
            Err(e) => {
                let chain_height = self.block_cache.get_chain_height().await?.0;
                match hash_or_height {
                    HashOrHeight::Height(Height(height)) if height >= chain_height => Err(
                        FetchServiceError::TonicStatusError(tonic::Status::out_of_range(format!(
                            "Error: Height out of range [{hash_or_height}]. Height requested \
                                is greater than the best chain tip [{chain_height}].",
                        ))),
                    ),
                    HashOrHeight::Height(height)
                        if height > self.data.network().sapling_activation_height() =>
                    {
                        Err(FetchServiceError::TonicStatusError(
                            tonic::Status::out_of_range(format!(
                                "Error: Height out of range [{hash_or_height}]. Height requested \
                                is below sapling activation height [{chain_height}].",
                            )),
                        ))
                    }
                    _otherwise =>
                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                    {
                        Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                            format!("Error: Failed to retrieve block from node. Server Error: {e}",),
                        )))
                    }
                }
            }
        }
    }

    /// Same as GetBlock except actions contain only nullifiers
    ///
    /// NOTE: Currently this only returns Orchard nullifiers to follow Lightwalletd functionality but Sapling could be added if required by wallets.
    async fn get_block_nullifiers(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        let height: u32 = match request.height.try_into() {
            Ok(height) => height,
            Err(_) => {
                return Err(FetchServiceError::TonicStatusError(
                    tonic::Status::invalid_argument(
                        "Error: Height out of range. Failed to convert to u32.",
                    ),
                ));
            }
        };
        match self
            .block_cache
            .get_compact_block_nullifiers(height.to_string())
            .await
        {
            Ok(block) => Ok(block),
            Err(e) => {
                let chain_height = self.block_cache.get_chain_height().await?.0;
                if height >= chain_height {
                    Err(FetchServiceError::TonicStatusError(tonic::Status::out_of_range(
                            format!(
                                "Error: Height out of range [{height}]. Height requested is greater than the best chain tip [{chain_height}].",
                            )
                        )))
                } else {
                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                    Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                        format!("Error: Failed to retrieve block from node. Server Error: {e}",),
                    )))
                }
            }
        }
    }

    /// Return a list of consecutive compact blocks
    #[allow(deprecated)]
    async fn get_block_range(
        &self,
        request: BlockRange,
    ) -> Result<CompactBlockStream, Self::Error> {
        let mut start: u32 = match request.start {
            Some(block_id) => match block_id.height.try_into() {
                Ok(height) => height,
                Err(_) => {
                    return Err(FetchServiceError::TonicStatusError(
                        tonic::Status::invalid_argument(
                            "Error: Start height out of range. Failed to convert to u32.",
                        ),
                    ));
                }
            },
            None => {
                return Err(FetchServiceError::TonicStatusError(
                    tonic::Status::invalid_argument("Error: No start height given."),
                ));
            }
        };
        let mut end: u32 = match request.end {
            Some(block_id) => match block_id.height.try_into() {
                Ok(height) => height,
                Err(_) => {
                    return Err(FetchServiceError::TonicStatusError(
                        tonic::Status::invalid_argument(
                            "Error: End height out of range. Failed to convert to u32.",
                        ),
                    ));
                }
            },
            None => {
                return Err(FetchServiceError::TonicStatusError(
                    tonic::Status::invalid_argument("Error: No start height given."),
                ));
            }
        };
        let rev_order = if start > end {
            (start, end) = (end, start);
            true
        } else {
            false
        };
        let chain_height = self.block_cache.get_chain_height().await?.0;
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.service.timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service.channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(time::Duration::from_secs((service_timeout*4) as u64), async {
                    for height in start..=end {
                        let height = if rev_order {
                            end - (height - start)
                        } else {
                            height
                        };
                        match fetch_service_clone.block_cache.get_compact_block(
                            height.to_string(),
                        ).await {
                            Ok(block) => {
                                if channel_tx.send(Ok(block)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if height >= chain_height {
                                    match channel_tx
                                        .send(Err(tonic::Status::out_of_range(format!(
                                            "Error: Height out of range [{height}]. Height requested is greater than the best chain tip [{chain_height}].",
                                        ))))
                                        .await

                                    {
                                        Ok(_) => break,
                                        Err(e) => {
                                            warn!("GetBlockRange channel closed unexpectedly: {}", e);
                                            break;
                                        }
                                    }
                                } else {
                                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                                    if channel_tx
                                        .send(Err(tonic::Status::unknown(e.to_string())))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                })
                .await;
            match timeout {
                Ok(_) => {}
                Err(_) => {
                    channel_tx
                        .send(Err(tonic::Status::deadline_exceeded(
                            "Error: get_block_range gRPC request timed out.",
                        )))
                        .await
                        .ok();
                }
            }
        });
        Ok(CompactBlockStream::new(channel_rx))
    }

    /// Same as GetBlockRange except actions contain only nullifiers
    ///
    /// NOTE: Currently this only returns Orchard nullifiers to follow Lightwalletd functionality but Sapling could be added if required by wallets.
    #[allow(deprecated)]
    async fn get_block_range_nullifiers(
        &self,
        request: BlockRange,
    ) -> Result<CompactBlockStream, Self::Error> {
        let tonic_status_error =
            |err| FetchServiceError::TonicStatusError(tonic::Status::invalid_argument(err));
        let mut start = match request.start {
            Some(block_id) => match u32::try_from(block_id.height) {
                Ok(height) => Ok(height),
                Err(_) => Err("Error: Start height out of range. Failed to convert to u32."),
            },
            None => Err("Error: No start height given."),
        }
        .map_err(tonic_status_error)?;
        let mut end = match request.end {
            Some(block_id) => match u32::try_from(block_id.height) {
                Ok(height) => Ok(height),
                Err(_) => Err("Error: End height out of range. Failed to convert to u32."),
            },
            None => Err("Error: No start height given."),
        }
        .map_err(tonic_status_error)?;
        let rev_order = if start > end {
            (start, end) = (end, start);
            true
        } else {
            false
        };
        let chain_height = self.block_cache.get_chain_height().await?.0;
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.service.timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service.channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    for height in start..=end {
                        let height = if rev_order {
                            end - (height - start)
                        } else {
                            height
                        };
                        if let Err(e) = channel_tx
                            .send(
                                fetch_service_clone
                                    .block_cache
                                    .get_compact_block_nullifiers(height.to_string())
                                    .await
                                    .map_err(|e| {
                                        if height >= chain_height {
                                            tonic::Status::out_of_range(format!(
                                            "Error: Height out of range [{height}]. Height requested \
                                            is greater than the best chain tip [{chain_height}].",
                                        ))
                                        } else {
                                            // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                                            tonic::Status::unknown(e.to_string())
                                        }
                                    }),
                            )
                            .await
                        {
                            warn!("GetBlockRangeNullifiers channel closed unexpectedly: {}", e);
                            break;
                        }
                    }
                },
            )
            .await;
            if timeout.is_err() {
                channel_tx
                    .send(Err(tonic::Status::deadline_exceeded(
                        "Error: get_block_range_nullifiers gRPC request timed out.",
                    )))
                    .await
                    .ok();
            }
        });
        Ok(CompactBlockStream::new(channel_rx))
    }

    /// Return the requested full (not compact) transaction (as from zcashd)
    async fn get_transaction(&self, request: TxFilter) -> Result<RawTransaction, Self::Error> {
        let hash = request.hash;
        if hash.len() == 32 {
            let reversed_hash = hash.iter().rev().copied().collect::<Vec<u8>>();
            let hash_hex = hex::encode(reversed_hash);
            let tx = self.get_raw_transaction(hash_hex, Some(1)).await?;

            let (hex, height) = if let GetRawTransaction::Object(tx_object) = tx {
                (tx_object.hex().clone(), tx_object.height())
            } else {
                return Err(FetchServiceError::TonicStatusError(
                    tonic::Status::not_found("Error: Transaction not received"),
                ));
            };
            let height: u64 = match height {
                Some(h) => h as u64,
                // Zebra returns None for mempool transactions, convert to `Mempool Height`.
                None => self.block_cache.get_chain_height().await?.0 as u64,
            };

            Ok(RawTransaction {
                data: hex.as_ref().to_vec(),
                height,
            })
        } else {
            Err(FetchServiceError::TonicStatusError(
                tonic::Status::invalid_argument("Error: Transaction hash incorrect"),
            ))
        }
    }

    /// Submit the given transaction to the Zcash network
    async fn send_transaction(&self, request: RawTransaction) -> Result<SendResponse, Self::Error> {
        let hex_tx = hex::encode(request.data);
        let tx_output = self.send_raw_transaction(hex_tx).await?;

        Ok(SendResponse {
            error_code: 0,
            error_message: tx_output.hash().to_string(),
        })
    }

    /// Return the txids corresponding to the given t-address within the given block range
    #[allow(deprecated)]
    async fn get_taddress_txids(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        let chain_height = self.chain_height().await?;
        let txids = self.get_taddress_txids_helper(request).await?;
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.service.timeout;
        let (transmitter, receiver) = mpsc::channel(self.config.service.channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    for txid in txids {
                        let transaction =
                            fetch_service_clone.get_raw_transaction(txid, Some(1)).await;
                        if handle_raw_transaction::<Self>(
                            chain_height.0 as u64,
                            transaction,
                            transmitter.clone(),
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                },
            )
            .await;
            match timeout {
                Ok(_) => {}
                Err(_) => {
                    transmitter
                        .send(Err(tonic::Status::internal(
                            "Error: get_taddress_txids gRPC request timed out",
                        )))
                        .await
                        .ok();
                }
            }
        });
        Ok(RawTransactionStream::new(receiver))
    }

    /// Returns the total balance for a list of taddrs
    async fn get_taddress_balance(&self, request: AddressList) -> Result<Balance, Self::Error> {
        let taddrs = AddressStrings::new(request.addresses);
        let balance = self.z_get_address_balance(taddrs).await?;
        let checked_balance: i64 = match i64::try_from(balance.balance()) {
            Ok(balance) => balance,
            Err(_) => {
                return Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                    "Error: Error converting balance from u64 to i64.",
                )));
            }
        };
        Ok(Balance {
            value_zat: checked_balance,
        })
    }

    /// Returns the total balance for a list of taddrs
    #[allow(deprecated)]
    async fn get_taddress_balance_stream(
        &self,
        mut request: AddressStream,
    ) -> Result<Balance, Self::Error> {
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.service.timeout;
        let (channel_tx, mut channel_rx) =
            mpsc::channel::<String>(self.config.service.channel_size as usize);
        let fetcher_task_handle = tokio::spawn(async move {
            let fetcher_timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    let mut total_balance: u64 = 0;
                    loop {
                        match channel_rx.recv().await {
                            Some(taddr) => {
                                let taddrs = AddressStrings::new(vec![taddr]);
                                let balance =
                                    fetch_service_clone.z_get_address_balance(taddrs).await?;
                                total_balance += balance.balance();
                            }
                            None => {
                                return Ok(total_balance);
                            }
                        }
                    }
                },
            )
            .await;
            match fetcher_timeout {
                Ok(result) => result,
                Err(_) => Err(tonic::Status::deadline_exceeded(
                    "Error: get_taddress_balance_stream request timed out.",
                )),
            }
        });
        // NOTE: This timeout is so slow due to the blockcache not being implemented. This should be reduced to 30s once functionality is in place.
        // TODO: Make [rpc_timout] a configurable system variable with [default = 30s] and [mempool_rpc_timout = 4*rpc_timeout]
        let addr_recv_timeout = timeout(
            time::Duration::from_secs((service_timeout * 4) as u64),
            async {
                while let Some(address_result) = request.next().await {
                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                    let address = address_result.map_err(|e| {
                        tonic::Status::unknown(format!("Failed to read from stream: {e}"))
                    })?;
                    if channel_tx.send(address.address).await.is_err() {
                        // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                        return Err(tonic::Status::unknown(
                            "Error: Failed to send address to balance task.",
                        ));
                    }
                }
                drop(channel_tx);
                Ok::<(), tonic::Status>(())
            },
        )
        .await;
        match addr_recv_timeout {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                fetcher_task_handle.abort();
                return Err(FetchServiceError::TonicStatusError(e));
            }
            Err(_) => {
                fetcher_task_handle.abort();
                return Err(FetchServiceError::TonicStatusError(
                    tonic::Status::deadline_exceeded(
                        "Error: get_taddress_balance_stream request timed out in address loop.",
                    ),
                ));
            }
        }
        match fetcher_task_handle.await {
            Ok(Ok(total_balance)) => {
                let checked_balance: i64 = match i64::try_from(total_balance) {
                    Ok(balance) => balance,
                    Err(_) => {
                        // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                        return Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                            "Error: Error converting balance from u64 to i64.",
                        )));
                    }
                };
                Ok(Balance {
                    value_zat: checked_balance,
                })
            }
            Ok(Err(e)) => Err(FetchServiceError::TonicStatusError(e)),
            // TODO: Hide server error from clients before release. Currently useful for dev purposes.
            Err(e) => Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                format!("Fetcher Task failed: {e}"),
            ))),
        }
    }

    /// Return the compact transactions currently in the mempool; the results
    /// can be a few seconds out of date. If the Exclude list is empty, return
    /// all transactions; otherwise return all *except* those in the Exclude list
    /// (if any); this allows the client to avoid receiving transactions that it
    /// already has (from an earlier call to this rpc). The transaction IDs in the
    /// Exclude list can be shortened to any number of bytes to make the request
    /// more bandwidth-efficient; if two or more transactions in the mempool
    /// match a shortened txid, they are all sent (none is excluded). Transactions
    /// in the exclude list that don't exist in the mempool are ignored.
    #[allow(deprecated)]
    async fn get_mempool_tx(
        &self,
        request: Exclude,
    ) -> Result<CompactTransactionStream, Self::Error> {
        let exclude_txids: Vec<String> = request
            .txid
            .iter()
            .map(|txid_bytes| {
                let reversed_txid_bytes: Vec<u8> = txid_bytes.iter().cloned().rev().collect();
                hex::encode(&reversed_txid_bytes)
            })
            .collect();

        let mempool = self.mempool.clone();
        let service_timeout = self.config.service.timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service.channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    for (mempool_key, mempool_value) in
                        mempool.get_filtered_mempool(exclude_txids).await
                    {
                        let txid_bytes = match hex::decode(mempool_key.txid) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                if channel_tx
                                    .send(Err(tonic::Status::unknown(error.to_string())))
                                    .await
                                    .is_err()
                                {
                                    break;
                                } else {
                                    continue;
                                }
                            }
                        };
                        match <FullTransaction as ParseFromSlice>::parse_from_slice(
                            mempool_value.serialized_tx.as_ref().as_ref(),
                            Some(vec![txid_bytes]),
                            None,
                        ) {
                            Ok(transaction) => {
                                // ParseFromSlice returns any data left after the conversion to a
                                // FullTransaction, If the conversion has succeeded this should be empty.
                                if transaction.0.is_empty() {
                                    if channel_tx
                                        .send(
                                            transaction
                                                .1
                                                .to_compact(0)
                                                .map_err(|e| tonic::Status::unknown(e.to_string())),
                                        )
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                } else {
                                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                                    if channel_tx
                                        .send(Err(tonic::Status::unknown("Error: ")))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                                if channel_tx
                                    .send(Err(tonic::Status::unknown(e.to_string())))
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
                        .send(Err(tonic::Status::internal(
                            "Error: get_mempool_tx gRPC request timed out",
                        )))
                        .await
                        .ok();
                }
            }
        });

        Ok(CompactTransactionStream::new(channel_rx))
    }

    /// Return a stream of current Mempool transactions. This will keep the output stream open while
    /// there are mempool transactions. It will close the returned stream when a new block is mined.
    #[allow(deprecated)]
    async fn get_mempool_stream(&self) -> Result<RawTransactionStream, Self::Error> {
        let mut mempool = self.mempool.clone();
        let service_timeout = self.config.service.timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service.channel_size as usize);
        let mempool_height = self.block_cache.get_chain_height().await?.0;
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 6) as u64),
                async {
                    let (mut mempool_stream, _mempool_handle) = match mempool
                        .get_mempool_stream(None)
                        .await
                    {
                        Ok(stream) => stream,
                        Err(e) => {
                            warn!("Error fetching stream from mempool: {:?}", e);
                            channel_tx
                                .send(Err(tonic::Status::internal("Error getting mempool stream")))
                                .await
                                .ok();
                            return;
                        }
                    };
                    while let Some(result) = mempool_stream.recv().await {
                        match result {
                            Ok((_mempool_key, mempool_value)) => {
                                if channel_tx
                                    .send(Ok(RawTransaction {
                                        data: mempool_value
                                            .serialized_tx
                                            .as_ref()
                                            .as_ref()
                                            .to_vec(),
                                        height: mempool_height as u64,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(e) => {
                                channel_tx
                                    .send(Err(tonic::Status::internal(format!(
                                        "Error in mempool stream: {e:?}"
                                    ))))
                                    .await
                                    .ok();
                                break;
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
                        .send(Err(tonic::Status::internal(
                            "Error: get_mempool_stream gRPC request timed out",
                        )))
                        .await
                        .ok();
                }
            }
        });

        Ok(RawTransactionStream::new(channel_rx))
    }

    /// GetTreeState returns the note commitment tree state corresponding to the given block.
    /// See section 3.7 of the Zcash protocol specification. It returns several other useful
    /// values also (even though they can be obtained using GetBlock).
    /// The block can be specified by either height or hash.
    async fn get_tree_state(&self, request: BlockId) -> Result<TreeState, Self::Error> {
        let chain_info = self.get_blockchain_info().await?;
        let hash_or_height = if request.height != 0 {
            match u32::try_from(request.height) {
                Ok(height) => {
                    if height > chain_info.blocks().0 {
                        return Err(FetchServiceError::TonicStatusError(tonic::Status::out_of_range(
                            format!(
                                "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                                height, chain_info.blocks().0,
                            ))
                        ));
                    } else {
                        height.to_string()
                    }
                }
                Err(_) => {
                    return Err(FetchServiceError::TonicStatusError(
                        tonic::Status::invalid_argument(
                            "Error: Height out of range. Failed to convert to u32.",
                        ),
                    ));
                }
            }
        } else {
            hex::encode(request.hash)
        };
        match self.z_get_treestate(hash_or_height).await {
            Ok(state) => {
                let (hash, height, time, sapling, orchard) = state.into_parts();
                Ok(TreeState {
                    network: chain_info.chain().clone(),
                    height: height.0 as u64,
                    hash: hash.to_string(),
                    time,
                    sapling_tree: sapling.map(hex::encode).unwrap_or_default(),
                    orchard_tree: orchard.map(hex::encode).unwrap_or_default(),
                })
            }
            Err(e) => {
                // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                    format!("Error: Failed to retrieve treestate from node. Server Error: {e}",),
                )))
            }
        }
    }

    /// GetLatestTreeState returns the note commitment tree state corresponding to the chain tip.
    async fn get_latest_tree_state(&self) -> Result<TreeState, Self::Error> {
        let chain_info = self.get_blockchain_info().await?;
        match self
            .z_get_treestate(chain_info.blocks().0.to_string())
            .await
        {
            Ok(state) => {
                let (hash, height, time, sapling, orchard) = state.into_parts();
                Ok(TreeState {
                    network: chain_info.chain().clone(),
                    height: height.0 as u64,
                    hash: hash.to_string(),
                    time,
                    sapling_tree: sapling.map(hex::encode).unwrap_or_default(),
                    orchard_tree: orchard.map(hex::encode).unwrap_or_default(),
                })
            }
            Err(e) => {
                // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                    format!("Error: Failed to retrieve treestate from node. Server Error: {e}",),
                )))
            }
        }
    }

    #[allow(deprecated)]
    fn timeout_channel_size(&self) -> (u32, u32) {
        (
            self.config.service.timeout,
            self.config.service.channel_size,
        )
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are collected and returned as a single Vec.
    async fn get_address_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<GetAddressUtxosReplyList, Self::Error> {
        let taddrs = AddressStrings::new(request.addresses);
        let utxos = self.z_get_address_utxos(taddrs).await?;
        let mut address_utxos: Vec<GetAddressUtxosReply> = Vec::new();
        let mut entries: u32 = 0;
        for utxo in utxos {
            let (address, tx_hash, output_index, script, satoshis, height) = utxo.into_parts();
            if (height.0 as u64) < request.start_height {
                continue;
            }
            entries += 1;
            if request.max_entries > 0 && entries > request.max_entries {
                break;
            }
            let checked_index = match i32::try_from(output_index.index()) {
                Ok(index) => index,
                Err(_) => {
                    return Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                        "Error: Index out of range. Failed to convert to i32.",
                    )));
                }
            };
            let checked_satoshis = match i64::try_from(satoshis) {
                Ok(satoshis) => satoshis,
                Err(_) => {
                    return Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                        "Error: Satoshis out of range. Failed to convert to i64.",
                    )));
                }
            };
            let utxo_reply = GetAddressUtxosReply {
                address: address.to_string(),
                txid: tx_hash.0.to_vec(),
                index: checked_index,
                script: script.as_raw_bytes().to_vec(),
                value_zat: checked_satoshis,
                height: height.0 as u64,
            };
            address_utxos.push(utxo_reply)
        }
        Ok(GetAddressUtxosReplyList { address_utxos })
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are returned in a stream.
    #[allow(deprecated)]
    async fn get_address_utxos_stream(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<UtxoReplyStream, Self::Error> {
        let taddrs = AddressStrings::new(request.addresses);
        let utxos = self.z_get_address_utxos(taddrs).await?;
        let service_timeout = self.config.service.timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service.channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    let mut entries: u32 = 0;
                    for utxo in utxos {
                        let (address, tx_hash, output_index, script, satoshis, height) =
                            utxo.into_parts();
                        if (height.0 as u64) < request.start_height {
                            continue;
                        }
                        entries += 1;
                        if request.max_entries > 0 && entries > request.max_entries {
                            break;
                        }
                        let checked_index = match i32::try_from(output_index.index()) {
                            Ok(index) => index,
                            Err(_) => {
                                let _ = channel_tx
                                    .send(Err(tonic::Status::unknown(
                                        "Error: Index out of range. Failed to convert to i32.",
                                    )))
                                    .await;
                                return;
                            }
                        };
                        let checked_satoshis = match i64::try_from(satoshis) {
                            Ok(satoshis) => satoshis,
                            Err(_) => {
                                let _ = channel_tx
                                    .send(Err(tonic::Status::unknown(
                                        "Error: Satoshis out of range. Failed to convert to i64.",
                                    )))
                                    .await;
                                return;
                            }
                        };
                        let utxo_reply = GetAddressUtxosReply {
                            address: address.to_string(),
                            txid: tx_hash.0.to_vec(),
                            index: checked_index,
                            script: script.as_raw_bytes().to_vec(),
                            value_zat: checked_satoshis,
                            height: height.0 as u64,
                        };
                        if channel_tx.send(Ok(utxo_reply)).await.is_err() {
                            return;
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
        Ok(UtxoReplyStream::new(channel_rx))
    }

    /// Return information about this lightwalletd instance and the blockchain
    async fn get_lightd_info(&self) -> Result<LightdInfo, Self::Error> {
        let blockchain_info = self.get_blockchain_info().await?;
        let sapling_id = zebra_rpc::methods::ConsensusBranchIdHex::new(
            zebra_chain::parameters::ConsensusBranchId::from_hex("76b809bb")
                .map_err(|_e| {
                    tonic::Status::internal(
                        "Internal Error - Consesnsus Branch ID hex conversion failed",
                    )
                })?
                .into(),
        );
        let sapling_activation_height = blockchain_info
            .upgrades()
            .get(&sapling_id)
            .map_or(Height(1), |sapling_json| sapling_json.into_parts().1);

        let consensus_branch_id = zebra_chain::parameters::ConsensusBranchId::from(
            blockchain_info.consensus().into_parts().0,
        )
        .to_string();

        Ok(LightdInfo {
            version: self.data.build_info().version(),
            vendor: "ZingoLabs ZainoD".to_string(),
            taddr_support: true,
            chain_name: blockchain_info.chain().clone(),
            sapling_activation_height: sapling_activation_height.0 as u64,
            consensus_branch_id,
            block_height: blockchain_info.blocks().0 as u64,
            git_commit: self.data.build_info().commit_hash(),
            branch: self.data.build_info().branch(),
            build_date: self.data.build_info().build_date(),
            build_user: self.data.build_info().build_user(),
            estimated_height: blockchain_info.estimated_height().0 as u64,
            zcashd_build: self.data.zebra_build(),
            zcashd_subversion: self.data.zebra_subversion(),
        })
    }

    /// Testing-only, requires lightwalletd --ping-very-insecure (do not enable in production)
    ///
    /// NOTE: Currently unimplemented in Zaino.
    async fn ping(&self, _request: Duration) -> Result<PingResponse, Self::Error> {
        Err(FetchServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Ping not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }
}
