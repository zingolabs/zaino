//! Zcash chain fetch and tx submission service backed by zcashds JsonRPC service.

use futures::StreamExt;
use hex::FromHex;
use std::time;
use tokio::{sync::mpsc, time::timeout};
use tonic::async_trait;
use tracing::{info, warn};

use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_rpc::methods::{
    trees::{GetSubtrees, GetTreestate},
    AddressBalance, AddressStrings, GetAddressTxIdsRequest, GetAddressUtxos, GetBlock,
    GetBlockChainInfo, GetInfo, GetRawTransaction, SentTransactionHash,
};

use zaino_fetch::{
    chain::{transaction::FullTransaction, utils::ParseFromSlice},
    jsonrpc::connector::{JsonRpcConnector, RpcError},
};
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, Balance, BlockId, BlockRange, Duration, Exclude, GetAddressUtxosArg,
        GetAddressUtxosReply, GetAddressUtxosReplyList, GetSubtreeRootsArg, LightdInfo,
        PingResponse, RawTransaction, SendResponse, ShieldedProtocol, SubtreeRoot,
        TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};

use crate::{
    config::FetchServiceConfig,
    error::FetchServiceError,
    indexer::{IndexerSubscriber, LightWalletIndexer, ZcashIndexer, ZcashService},
    local_cache::{BlockCache, BlockCacheSubscriber},
    mempool::{Mempool, MempoolSubscriber},
    status::StatusType,
    stream::{
        AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
        SubtreeRootReplyStream, UtxoReplyStream,
    },
    utils::{get_build_info, ServiceMetadata},
};

/// Chain fetch service backed by Zcashd's JsonRPC engine.
///
/// This service is a central service, [`FetchServiceSubscriber`] should be created to fetch data.
/// This is done to enable large numbers of concurrent subscribers without significant slowdowns.
///
/// NOTE: We currently do not implement clone for chain fetch services as this service is responsible for maintaining and closing its child processes.
///       ServiceSubscribers are used to create separate chain fetch processes while allowing central state processes to be managed in a single place.
///       If we want the ability to clone Service all JoinHandle's should be converted to Arc<JoinHandle>.
#[derive(Debug)]
pub struct FetchService {
    /// JsonRPC Client.
    fetcher: JsonRpcConnector,
    /// Local compact block cache.
    block_cache: BlockCache,
    /// Internal mempool.
    mempool: Mempool,
    /// Service metadata.
    data: ServiceMetadata,
    /// StateService config data.
    config: FetchServiceConfig,
}

#[async_trait]
impl ZcashService for FetchService {
    type Error = FetchServiceError;
    type Subscriber = FetchServiceSubscriber;
    type Config = FetchServiceConfig;
    /// Initializes a new StateService instance and starts sync process.
    async fn spawn(config: FetchServiceConfig) -> Result<Self, FetchServiceError> {
        info!("Launching Chain Fetch Service..");

        let fetcher = JsonRpcConnector::new_from_config_parts(
            config.validator_cookie_auth,
            config.validator_rpc_address,
            config.validator_rpc_user.clone(),
            config.validator_rpc_password.clone(),
            config.validator_cookie_path.clone(),
        )
        .await?;

        let zebra_build_data = fetcher.get_info().await?;
        let data = ServiceMetadata::new(
            get_build_info(),
            config.network.clone(),
            zebra_build_data.build,
            zebra_build_data.subversion,
        );

        let block_cache = BlockCache::spawn(&fetcher, config.clone().into()).await?;

        let mempool = Mempool::spawn(&fetcher, None).await?;

        let fetch_service = Self {
            fetcher,
            block_cache,
            mempool,
            data,
            config,
        };

        while fetch_service.status() != StatusType::Ready {
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
    fn status(&self) -> StatusType {
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

impl Drop for FetchService {
    fn drop(&mut self) {
        self.close()
    }
}

/// A fetch service subscriber.
///
/// Subscribers should be
#[derive(Debug, Clone)]
pub struct FetchServiceSubscriber {
    /// JsonRPC Client.
    fetcher: JsonRpcConnector,
    /// Local compact block cache.
    block_cache: BlockCacheSubscriber,
    /// Internal mempool.
    mempool: MempoolSubscriber,
    /// Service metadata.
    data: ServiceMetadata,
    /// StateService config data.
    config: FetchServiceConfig,
}

impl FetchServiceSubscriber {
    /// Fetches the current status
    pub fn status(&self) -> StatusType {
        let mempool_status = self.mempool.status();
        let block_cache_status = self.block_cache.status();

        mempool_status.combine(block_cache_status)
    }
}

#[async_trait]
impl ZcashIndexer for FetchServiceSubscriber {
    type Error = FetchServiceError;

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
    async fn get_info(&self) -> Result<GetInfo, Self::Error> {
        Ok(GetInfo::from_parts(
            self.data.zebra_build(),
            self.data.zebra_subversion(),
        ))
    }

    /// Returns blockchain state information, as a [`GetBlockChainInfo`] JSON struct.
    ///
    /// zcashd reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Notes
    ///
    /// Some fields from the zcashd reference are missing from Zebra's [`GetBlockChainInfo`]. It only contains the fields
    /// [required for lightwalletd support.](https://github.com/zcash/lightwalletd/blob/v0.4.9/common/common.go#L72-L89)
    async fn get_blockchain_info(&self) -> Result<GetBlockChainInfo, Self::Error> {
        Ok(self.fetcher.get_blockchain_info().await?.into())
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
            .map(|(key, _)| key.0)
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
    async fn z_get_treestate(&self, hash_or_height: String) -> Result<GetTreestate, Self::Error> {
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
    ) -> Result<GetSubtrees, Self::Error> {
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
}

#[async_trait]
impl LightWalletIndexer for FetchServiceSubscriber {
    type Error = FetchServiceError;

    /// Return the height of the tip of the best chain
    async fn get_latest_block(&self) -> Result<BlockId, Self::Error> {
        let latest_height = self.block_cache.get_chain_height().await?;
        let mut latest_hash = self
            .block_cache
            .get_compact_block(latest_height.0.to_string())
            .await?
            .hash;
        latest_hash.reverse();

        Ok(BlockId {
            height: latest_height.0 as u64,
            hash: latest_hash,
        })
    }

    /// Return the compact block corresponding to the given block identifier
    async fn get_block(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
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
        match self.block_cache.get_compact_block(height.to_string()).await {
            Ok(block) => Ok(block),
            Err(e) => {
                let chain_height = self.block_cache.get_chain_height().await?.0;
                if height >= chain_height {
                    Err(FetchServiceError::TonicStatusError(tonic::Status::out_of_range(
                            format!(
                                "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                                height, chain_height,
                            )
                        )))
                } else {
                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                    Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                        format!(
                            "Error: Failed to retrieve block from node. Server Error: {}",
                            e,
                        ),
                    )))
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
                                "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                                height, chain_height,
                            )
                        )))
                } else {
                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                    Err(FetchServiceError::TonicStatusError(tonic::Status::unknown(
                        format!(
                            "Error: Failed to retrieve block from node. Server Error: {}",
                            e,
                        ),
                    )))
                }
            }
        }
    }

    /// Return a list of consecutive compact blocks
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
        let service_timeout = self.config.service_timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
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
                                            "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                                            height, chain_height,
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
        let service_timeout = self.config.service_timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
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
                                            "Error: Height out of range [{}]. Height requested \
                                            is greater than the best chain tip [{}].",
                                            height, chain_height,
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
                (tx_object.hex, tx_object.height)
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
            error_message: tx_output.inner().to_string(),
        })
    }

    /// Return the txids corresponding to the given t-address within the given block range
    async fn get_taddress_txids(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        let chain_height = self.block_cache.get_chain_height().await?.0;
        let (start, end) =
            match request.range {
                Some(range) => match (range.start, range.end) {
                    (Some(start), Some(end)) => {
                        let start = match u32::try_from(start.height) {
                            Ok(height) => height.min(chain_height),
                            Err(_) => return Err(FetchServiceError::TonicStatusError(
                                tonic::Status::invalid_argument(
                                    "Error: Start height out of range. Failed to convert to u32.",
                                ),
                            )),
                        };
                        let end =
                            match u32::try_from(end.height) {
                                Ok(height) => height.min(chain_height),
                                Err(_) => return Err(FetchServiceError::TonicStatusError(
                                    tonic::Status::invalid_argument(
                                        "Error: End height out of range. Failed to convert to u32.",
                                    ),
                                )),
                            };
                        if start > end {
                            (end, start)
                        } else {
                            (start, end)
                        }
                    }
                    _ => {
                        return Err(FetchServiceError::TonicStatusError(
                            tonic::Status::invalid_argument("Error: Incomplete block range given."),
                        ))
                    }
                },
                None => {
                    return Err(FetchServiceError::TonicStatusError(
                        tonic::Status::invalid_argument("Error: No block range given."),
                    ))
                }
            };
        let txids = self
            .get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
                vec![request.address],
                start,
                end,
            ))
            .await?;
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.service_timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    for txid in txids {
                        let transaction =
                            fetch_service_clone.get_raw_transaction(txid, Some(1)).await;
                        match transaction {
                            Ok(GetRawTransaction::Object(transaction_obj)) => {
                                let height: u64 = match transaction_obj.height {
                                    Some(h) => h as u64,
                                    // Zebra returns None for mempool transactions, convert to `Mempool Height`.
                                    None => chain_height as u64,
                                };
                                if channel_tx
                                    .send(Ok(RawTransaction {
                                        data: transaction_obj.hex.as_ref().to_vec(),
                                        height,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Ok(GetRawTransaction::Raw(_)) => {
                                if channel_tx
                                    .send(Err(tonic::Status::unknown(
                                    "Received raw transaction type, this should not be impossible.",
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
                            "Error: get_taddress_txids gRPC request timed out",
                        )))
                        .await
                        .ok();
                }
            }
        });
        Ok(RawTransactionStream::new(channel_rx))
    }

    /// Returns the total balance for a list of taddrs
    async fn get_taddress_balance(&self, request: AddressList) -> Result<Balance, Self::Error> {
        let taddrs = AddressStrings::new_valid(request.addresses).map_err(|err_obj| {
            FetchServiceError::RpcError(RpcError::new_from_errorobject(
                err_obj,
                "Error in Validator",
            ))
        })?;
        let balance = self.z_get_address_balance(taddrs).await?;
        let checked_balance: i64 = match i64::try_from(balance.balance) {
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
    async fn get_taddress_balance_stream(
        &self,
        mut request: AddressStream,
    ) -> Result<Balance, Self::Error> {
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.service_timeout;
        let (channel_tx, mut channel_rx) =
            mpsc::channel::<String>(self.config.service_channel_size as usize);
        let fetcher_task_handle = tokio::spawn(async move {
            let fetcher_timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    let mut total_balance: u64 = 0;
                    loop {
                        match channel_rx.recv().await {
                            Some(taddr) => {
                                let taddrs =
                                    AddressStrings::new_valid(vec![taddr]).map_err(|err_obj| {
                                        FetchServiceError::RpcError(RpcError::new_from_errorobject(
                                            err_obj,
                                            "Error in Validator",
                                        ))
                                    })?;
                                let balance =
                                    fetch_service_clone.z_get_address_balance(taddrs).await?;
                                total_balance += balance.balance;
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
                        tonic::Status::unknown(format!("Failed to read from stream: {}", e))
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
                format!("Fetcher Task failed: {}", e),
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
        let service_timeout = self.config.service_timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout*4) as u64),
                async {
                    for (txid, transaction) in mempool.get_filtered_mempool(exclude_txids).await {
                        match transaction.0 {
                            GetRawTransaction::Object(transaction_object) => {
                                let txid_bytes = match hex::decode(txid.0) {
                                    Ok(bytes) => bytes,
                                    Err(e) => {
                                        if channel_tx
                                            .send(Err(tonic::Status::unknown(e.to_string())))
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
                                    transaction_object.hex.as_ref(),
                                    Some(vec!(txid_bytes)), None)
                                {
                                    Ok(transaction) => {
                                        // ParseFromSlice returns any data left after the conversion to a
                                        // FullTransaction, If the conversion has succeeded this should be empty.
                                        if transaction.0.is_empty() {
                                            if channel_tx.send(
                                                transaction
                                                .1
                                                .to_compact(0)
                                                .map_err(|e| {
                                                    tonic::Status::unknown(
                                                        e.to_string()
                                                    )
                                                })
                                            ).await.is_err() {
                                                break
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
                            GetRawTransaction::Raw(_) => {
                                if channel_tx
                                    .send(Err(tonic::Status::internal(
                                        "Error: Received raw transaction type, this should not be impossible.",
                                    )))
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
    async fn get_mempool_stream(&self) -> Result<RawTransactionStream, Self::Error> {
        let mut mempool = self.mempool.clone();
        let service_timeout = self.config.service_timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
        let mempool_height = self.block_cache.get_chain_height().await?.0;
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout*6) as u64),
                async {
                    let (mut mempool_stream, _mempool_handle) =
                        match mempool.get_mempool_stream().await {
                            Ok(stream) => stream,
                            Err(e) => {
                                warn!("Error fetching stream from mempool: {:?}", e);
                                channel_tx
                                    .send(Err(tonic::Status::internal(
                                        "Error getting mempool stream",
                                    )))
                                    .await
                                    .ok();
                                return;
                            }
                        };
                    while let Some(result) = mempool_stream.recv().await {
                        match result {
                            Ok((_mempool_key, mempool_value)) => {
                                match mempool_value.0 {
                                    GetRawTransaction::Object(transaction_object) => {
                                        if channel_tx
                                            .send(Ok(RawTransaction {
                                                data: transaction_object.hex.as_ref().to_vec(),
                                                height: mempool_height as u64,
                                            }))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    GetRawTransaction::Raw(_) => {
                                        if channel_tx
                                            .send(Err(tonic::Status::internal(
                                                "Error: Received raw transaction type, this should not be impossible.",
                                            )))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                channel_tx
                                    .send(Err(tonic::Status::internal(format!(
                                        "Error in mempool stream: {:?}",
                                        e
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
                    network: chain_info.chain(),
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
                    format!(
                        "Error: Failed to retrieve treestate from node. Server Error: {}",
                        e,
                    ),
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
                    network: chain_info.chain(),
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
                    format!(
                        "Error: Failed to retrieve treestate from node. Server Error: {}",
                        e,
                    ),
                )))
            }
        }
    }

    /// Returns a stream of information about roots of subtrees of the Sapling and Orchard
    /// note commitment trees.
    async fn get_subtree_roots(
        &self,
        request: GetSubtreeRootsArg,
    ) -> Result<SubtreeRootReplyStream, Self::Error> {
        let pool = match ShieldedProtocol::try_from(request.shielded_protocol) {
            Ok(protocol) => protocol.as_str_name(),
            Err(_) => {
                return Err(FetchServiceError::TonicStatusError(
                    tonic::Status::invalid_argument("Error: Invalid shielded protocol value."),
                ))
            }
        };
        let start_index = match u16::try_from(request.start_index) {
            Ok(value) => value,
            Err(_) => {
                return Err(FetchServiceError::TonicStatusError(
                    tonic::Status::invalid_argument("Error: start_index value exceeds u16 range."),
                ))
            }
        };
        let limit = if request.max_entries == 0 {
            None
        } else {
            match u16::try_from(request.max_entries) {
                Ok(value) => Some(value),
                Err(_) => {
                    return Err(FetchServiceError::TonicStatusError(
                        tonic::Status::invalid_argument(
                            "Error: max_entries value exceeds u16 range.",
                        ),
                    ))
                }
            }
        };
        let subtrees = self
            .z_get_subtrees_by_index(
                pool.to_string(),
                NoteCommitmentSubtreeIndex(start_index),
                limit.map(NoteCommitmentSubtreeIndex),
            )
            .await?;
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.service_timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    for subtree in subtrees.subtrees {
                        match fetch_service_clone
                            .z_get_block(subtree.end_height.0.to_string(), Some(1))
                            .await
                        {
                            Ok(GetBlock::Object { hash, height, .. }) => {
                                let checked_height = match height {
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
                                                    "GetSubtreeRoots channel closed unexpectedly: {}",
                                                    e
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
                                                "Error: Failed to hex decode root hash: {}.",
                                                e
                                            ))))
                                            .await
                                        {
                                            Ok(_) => break,
                                            Err(e) => {
                                                warn!(
                                                    "GetSubtreeRoots channel closed unexpectedly: {}",
                                                    e
                                                );
                                                break;
                                            }
                                        }
                                    }
                                };
                                if channel_tx
                                    .send(Ok(SubtreeRoot {
                                        root_hash: checked_root_hash,
                                        completing_block_hash: hash
                                            .0
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

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are collected and returned as a single Vec.
    async fn get_address_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<GetAddressUtxosReplyList, Self::Error> {
        let taddrs = AddressStrings::new_valid(request.addresses).map_err(|err_obj| {
            FetchServiceError::RpcError(RpcError::new_from_errorobject(
                err_obj,
                "Error in Validator",
            ))
        })?;
        let utxos = self.z_get_address_utxos(taddrs).await?;
        let mut address_utxos: Vec<GetAddressUtxosReply> = Vec::new();
        let mut entries: u32 = 0;
        for utxo in utxos {
            let (address, txid, output_index, script, satoshis, height) = utxo.into_parts();
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
                txid: txid.0.to_vec(),
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
    async fn get_address_utxos_stream(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<UtxoReplyStream, Self::Error> {
        let taddrs = AddressStrings::new_valid(request.addresses).map_err(|err_obj| {
            FetchServiceError::RpcError(RpcError::new_from_errorobject(
                err_obj,
                "Error in Validator",
            ))
        })?;
        let utxos = self.z_get_address_utxos(taddrs).await?;
        let service_timeout = self.config.service_timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    let mut entries: u32 = 0;
                    for utxo in utxos {
                        let (address, txid, output_index, script, satoshis, height) =
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
                            txid: txid.0.to_vec(),
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
            .map_or(zebra_chain::block::Height(1), |sapling_json| {
                sapling_json.into_parts().1
            });

        let consensus_branch_id = zebra_chain::parameters::ConsensusBranchId::from(
            blockchain_info.consensus().into_parts().0,
        )
        .to_string();

        Ok(LightdInfo {
            version: self.data.build_info().version(),
            vendor: "ZingoLabs ZainoD".to_string(),
            taddr_support: true,
            chain_name: blockchain_info.chain(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_fetch::jsonrpc::connector::test_node_and_return_url;
    use zaino_testutils::{TestManager, ZCASHD_CHAIN_CACHE_BIN, ZEBRAD_CHAIN_CACHE_BIN};
    use zebra_chain::parameters::Network;
    use zingo_infra_services::validator::Validator;

    #[tokio::test]
    async fn launch_fetch_service_zcashd_regtest_no_cache() {
        launch_fetch_service("zcashd", None).await;
    }

    #[tokio::test]
    async fn launch_fetch_service_zcashd_regtest_with_cache() {
        launch_fetch_service("zcashd", ZCASHD_CHAIN_CACHE_BIN.clone()).await;
    }

    #[tokio::test]
    async fn launch_fetch_service_zebrad_regtest_no_cache() {
        launch_fetch_service("zebrad", None).await;
    }

    #[tokio::test]
    async fn launch_fetch_service_zebrad_regtest_with_cache() {
        launch_fetch_service("zebrad", ZEBRAD_CHAIN_CACHE_BIN.clone()).await;
    }

    async fn create_test_manager_and_fetch_service(
        validator: &str,
        chain_cache: Option<std::path::PathBuf>,
        enable_zaino: bool,
        zaino_no_sync: bool,
        zaino_no_db: bool,
        enable_clients: bool,
    ) -> (TestManager, FetchService, FetchServiceSubscriber) {
        let test_manager = TestManager::launch(
            validator,
            None,
            chain_cache,
            enable_zaino,
            zaino_no_sync,
            zaino_no_db,
            enable_clients,
        )
        .await
        .unwrap();

        let fetch_service = FetchService::spawn(FetchServiceConfig::new(
            test_manager.zebrad_rpc_listen_address,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            test_manager
                .local_net
                .data_dir()
                .path()
                .to_path_buf()
                .join("zaino"),
            None,
            Network::new_regtest(Some(1), Some(1)),
            true,
            true,
        ))
        .await
        .unwrap();
        let subscriber = fetch_service.get_subscriber().inner();
        (test_manager, fetch_service, subscriber)
    }

    async fn launch_fetch_service(validator: &str, chain_cache: Option<std::path::PathBuf>) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, chain_cache, false, true, true, false)
                .await;
        assert_eq!(fetch_service_subscriber.status(), StatusType::Ready);
        dbg!(fetch_service_subscriber.data.clone());
        dbg!(fetch_service_subscriber.get_info().await.unwrap());
        dbg!(fetch_service_subscriber
            .get_blockchain_info()
            .await
            .unwrap()
            .blocks());

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_address_balance_zcashd() {
        fetch_service_get_address_balance("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_address_balance_zebrad() {
        fetch_service_get_address_balance("zebrad").await;
    }

    async fn fetch_service_get_address_balance(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");
        let recipient_address = clients.get_recipient_address("transparent").await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(recipient_address.as_str(), 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        clients.recipient.do_sync(true).await.unwrap();
        let recipient_balance = clients.recipient.do_balance().await;

        let fetch_service_balance = fetch_service_subscriber
            .z_get_address_balance(AddressStrings::new_valid(vec![recipient_address]).unwrap())
            .await
            .unwrap();

        dbg!(recipient_balance.clone());
        dbg!(fetch_service_balance);

        assert_eq!(recipient_balance.transparent_balance.unwrap(), 250_000,);
        assert_eq!(
            recipient_balance.transparent_balance.unwrap(),
            fetch_service_balance.balance,
        );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_raw_zcashd() {
        fetch_service_get_block_raw("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_raw_zebrad() {
        fetch_service_get_block_raw("zebrad").await;
    }

    async fn fetch_service_get_block_raw(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, false, true, true, false).await;

        dbg!(fetch_service_subscriber
            .z_get_block("1".to_string(), Some(0))
            .await
            .unwrap());

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_object_zcashd() {
        fetch_service_get_block_object("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_object_zebrad() {
        fetch_service_get_block_object("zebrad").await;
    }

    async fn fetch_service_get_block_object(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, false, true, true, false).await;

        dbg!(fetch_service_subscriber
            .z_get_block("1".to_string(), Some(1))
            .await
            .unwrap());

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_raw_mempool_zcashd() {
        fetch_service_get_raw_mempool("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_raw_mempool_zebrad() {
        fetch_service_get_raw_mempool("zebrad").await;
    }

    async fn fetch_service_get_raw_mempool(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        let json_service = JsonRpcConnector::new_with_basic_auth(
            test_node_and_return_url(
                test_manager.zebrad_rpc_listen_address,
                false,
                None,
                Some("xxxxxx".to_string()),
                Some("xxxxxx".to_string()),
            )
            .await
            .unwrap(),
            "xxxxxx".to_string(),
            "xxxxxx".to_string(),
        )
        .unwrap();

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let mut fetch_service_mempool = fetch_service_subscriber.get_raw_mempool().await.unwrap();
        let mut json_service_mempool = json_service.get_raw_mempool().await.unwrap().transactions;

        dbg!(&fetch_service_mempool);
        dbg!(&json_service_mempool);
        json_service_mempool.sort();
        fetch_service_mempool.sort();
        assert_eq!(json_service_mempool, fetch_service_mempool);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_z_get_treestate_zcashd() {
        fetch_service_z_get_treestate("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_z_get_treestate_zebrad() {
        fetch_service_z_get_treestate("zebrad").await;
    }

    async fn fetch_service_z_get_treestate(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        dbg!(fetch_service_subscriber
            .z_get_treestate("2".to_string())
            .await
            .unwrap());

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_z_get_subtrees_by_index_zcashd() {
        fetch_service_z_get_subtrees_by_index("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_z_get_subtrees_by_index_zebrad() {
        fetch_service_z_get_subtrees_by_index("zebrad").await;
    }

    async fn fetch_service_z_get_subtrees_by_index(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        dbg!(fetch_service_subscriber
            .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
            .await
            .unwrap());

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_raw_transaction_zcashd() {
        fetch_service_get_raw_transaction("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_raw_transaction_zebrad() {
        fetch_service_get_raw_transaction("zebrad").await;
    }

    async fn fetch_service_get_raw_transaction(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let tx = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        dbg!(fetch_service_subscriber
            .get_raw_transaction(tx.first().to_string(), Some(1))
            .await
            .unwrap());

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_address_tx_ids_zcashd() {
        fetch_service_get_address_tx_ids("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_address_tx_ids_zebrad() {
        fetch_service_get_address_tx_ids("zebrad").await;
    }

    async fn fetch_service_get_address_tx_ids(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");
        let recipient_address = clients.get_recipient_address("transparent").await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let tx = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(recipient_address.as_str(), 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let chain_height = fetch_service_subscriber
            .block_cache
            .get_chain_height()
            .await
            .unwrap()
            .0;
        dbg!(&chain_height);

        let fetch_service_txids = fetch_service_subscriber
            .get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
                vec![recipient_address],
                chain_height - 2,
                chain_height,
            ))
            .await
            .unwrap();

        dbg!(&tx);
        dbg!(&fetch_service_txids);
        assert_eq!(tx.first().to_string(), fetch_service_txids[0]);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_address_utxos_zcashd() {
        fetch_service_get_address_utxos("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_address_utxos_zebrad() {
        fetch_service_get_address_utxos("zebrad").await;
    }

    async fn fetch_service_get_address_utxos(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");
        let recipient_address = clients.get_recipient_address("transparent").await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let txid_1 = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(recipient_address.as_str(), 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        clients.faucet.do_sync(true).await.unwrap();

        let fetch_service_utxos = fetch_service_subscriber
            .z_get_address_utxos(AddressStrings::new_valid(vec![recipient_address]).unwrap())
            .await
            .unwrap();
        let (_, fetch_service_txid, ..) = fetch_service_utxos[0].into_parts();

        dbg!(&txid_1);
        dbg!(&fetch_service_utxos);
        assert_eq!(txid_1.first().to_string(), fetch_service_txid.to_string());

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_latest_block_zcashd() {
        fetch_service_get_latest_block("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_latest_block_zebrad() {
        fetch_service_get_latest_block("zebrad").await;
    }

    async fn fetch_service_get_latest_block(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let json_service = JsonRpcConnector::new_with_basic_auth(
            test_node_and_return_url(
                test_manager.zebrad_rpc_listen_address,
                false,
                None,
                Some("xxxxxx".to_string()),
                Some("xxxxxx".to_string()),
            )
            .await
            .unwrap(),
            "xxxxxx".to_string(),
            "xxxxxx".to_string(),
        )
        .unwrap();

        let fetch_service_get_latest_block =
            dbg!(fetch_service_subscriber.get_latest_block().await.unwrap());

        let json_service_blockchain_info = json_service.get_blockchain_info().await.unwrap();

        let json_service_get_latest_block = dbg!(BlockId {
            height: json_service_blockchain_info.blocks.0 as u64,
            hash: json_service_blockchain_info
                .best_block_hash
                .bytes_in_display_order()
                .to_vec(),
        });

        assert_eq!(fetch_service_get_latest_block.height, 2);
        assert_eq!(
            fetch_service_get_latest_block,
            json_service_get_latest_block
        );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_zcashd() {
        fetch_service_get_block("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_zebrad() {
        fetch_service_get_block("zebrad").await;
    }

    async fn fetch_service_get_block(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let block_id = BlockId {
            height: 1,
            hash: Vec::new(),
        };

        let fetch_service_get_block = dbg!(fetch_service_subscriber
            .get_block(block_id.clone())
            .await
            .unwrap());

        assert_eq!(fetch_service_get_block.height, block_id.height);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_nullifiers_zcashd() {
        fetch_service_get_block_nullifiers("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_nullifiers_zebrad() {
        fetch_service_get_block_nullifiers("zebrad").await;
    }

    async fn fetch_service_get_block_nullifiers(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let block_id = BlockId {
            height: 1,
            hash: Vec::new(),
        };

        let fetch_service_get_block_nullifiers = dbg!(fetch_service_subscriber
            .get_block_nullifiers(block_id.clone())
            .await
            .unwrap());

        assert_eq!(fetch_service_get_block_nullifiers.height, block_id.height);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_range_zcashd() {
        fetch_service_get_block_range("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_range_zebrad() {
        fetch_service_get_block_range("zebrad").await;
    }

    async fn fetch_service_get_block_range(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
        test_manager.local_net.generate_blocks(10).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let block_range = BlockRange {
            start: Some(BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            end: Some(BlockId {
                height: 10,
                hash: Vec::new(),
            }),
        };

        let fetch_service_stream = fetch_service_subscriber
            .get_block_range(block_range.clone())
            .await
            .unwrap();
        let fetch_service_compact_blocks: Vec<_> = fetch_service_stream.collect().await;

        let fetch_blocks: Vec<_> = fetch_service_compact_blocks
            .into_iter()
            .filter_map(|result| result.ok())
            .collect();

        dbg!(fetch_blocks);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_range_nullifiers_zcashd() {
        fetch_service_get_block_range_nullifiers("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_block_range_nullifiers_zebrad() {
        fetch_service_get_block_range_nullifiers("zebrad").await;
    }

    async fn fetch_service_get_block_range_nullifiers(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
        test_manager.local_net.generate_blocks(10).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let block_range = BlockRange {
            start: Some(BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            end: Some(BlockId {
                height: 10,
                hash: Vec::new(),
            }),
        };

        let fetch_service_stream = fetch_service_subscriber
            .get_block_range_nullifiers(block_range.clone())
            .await
            .unwrap();
        let fetch_service_compact_blocks: Vec<_> = fetch_service_stream.collect().await;

        let fetch_nullifiers: Vec<_> = fetch_service_compact_blocks
            .into_iter()
            .filter_map(|result| result.ok())
            .collect();

        dbg!(fetch_nullifiers);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_transaction_mined_zcashd() {
        fetch_service_get_transaction_mined("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_transaction_mined_zebrad() {
        fetch_service_get_transaction_mined("zebrad").await;
    }

    async fn fetch_service_get_transaction_mined(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let tx = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let tx_filter = TxFilter {
            block: None,
            index: 0,
            hash: tx.first().as_ref().to_vec(),
        };

        let fetch_service_get_transaction = dbg!(fetch_service_subscriber
            .get_transaction(tx_filter.clone())
            .await
            .unwrap());

        dbg!(fetch_service_get_transaction);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_transaction_mempool_zcashd() {
        fetch_service_get_transaction_mempool("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_transaction_mempool_zebrad() {
        fetch_service_get_transaction_mempool("zebrad").await;
    }

    async fn fetch_service_get_transaction_mempool(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let tx = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        let tx_filter = TxFilter {
            block: None,
            index: 0,
            hash: tx.first().as_ref().to_vec(),
        };

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let fetch_service_get_transaction = dbg!(fetch_service_subscriber
            .get_transaction(tx_filter.clone())
            .await
            .unwrap());

        dbg!(fetch_service_get_transaction);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_taddress_txids_zcashd() {
        fetch_service_get_taddress_txids("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_taddress_txids_zebrad() {
        fetch_service_get_taddress_txids("zebrad").await;
    }

    async fn fetch_service_get_taddress_txids(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");
        let recipient_address = clients.get_recipient_address("transparent").await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let tx = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(&recipient_address, 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let chain_height = fetch_service_subscriber
            .block_cache
            .get_chain_height()
            .await
            .unwrap()
            .0;
        dbg!(&chain_height);

        let block_filter = TransparentAddressBlockFilter {
            address: recipient_address,
            range: Some(BlockRange {
                start: Some(BlockId {
                    height: (chain_height - 2) as u64,
                    hash: Vec::new(),
                }),
                end: Some(BlockId {
                    height: chain_height as u64,
                    hash: Vec::new(),
                }),
            }),
        };

        let fetch_service_stream = fetch_service_subscriber
            .get_taddress_txids(block_filter.clone())
            .await
            .unwrap();
        let fetch_service_tx: Vec<_> = fetch_service_stream.collect().await;

        let fetch_tx: Vec<_> = fetch_service_tx
            .into_iter()
            .filter_map(|result| result.ok())
            .collect();

        dbg!(tx);
        dbg!(&fetch_tx);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_taddress_balance_zcashd() {
        fetch_service_get_taddress_balance("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_taddress_balance_zebrad() {
        fetch_service_get_taddress_balance("zebrad").await;
    }

    async fn fetch_service_get_taddress_balance(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");
        let recipient_address = clients.get_recipient_address("transparent").await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(&recipient_address, 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        clients.recipient.do_sync(true).await.unwrap();
        let balance = clients.recipient.do_balance().await;

        let address_list = AddressList {
            addresses: vec![recipient_address],
        };

        let fetch_service_balance = fetch_service_subscriber
            .get_taddress_balance(address_list.clone())
            .await
            .unwrap();

        dbg!(&fetch_service_balance);
        assert_eq!(
            fetch_service_balance.value_zat as u64,
            balance.transparent_balance.unwrap()
        );

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_mempool_tx_zcashd() {
        fetch_service_get_mempool_tx("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_mempool_tx_zebrad() {
        fetch_service_get_mempool_tx("zebrad").await;
    }

    async fn fetch_service_get_mempool_tx(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let tx_1 = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        let tx_2 = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let exclude_list_empty = Exclude { txid: Vec::new() };

        let fetch_service_stream = fetch_service_subscriber
            .get_mempool_tx(exclude_list_empty.clone())
            .await
            .unwrap();
        let fetch_service_mempool_tx: Vec<_> = fetch_service_stream.collect().await;

        let fetch_mempool_tx: Vec<_> = fetch_service_mempool_tx
            .into_iter()
            .filter_map(|result| result.ok())
            .collect();

        let mut sorted_fetch_mempool_tx = fetch_mempool_tx.clone();
        sorted_fetch_mempool_tx.sort_by_key(|tx| tx.hash.clone());

        let mut tx1_bytes = *tx_1.first().as_ref();
        tx1_bytes.reverse();
        let mut tx2_bytes = *tx_2.first().as_ref();
        tx2_bytes.reverse();

        let mut sorted_txids = [tx1_bytes, tx2_bytes];
        sorted_txids.sort_by_key(|hash| *hash);

        assert_eq!(sorted_fetch_mempool_tx[0].hash, sorted_txids[0]);
        assert_eq!(sorted_fetch_mempool_tx[1].hash, sorted_txids[1]);

        let exclude_list = Exclude {
            txid: vec![sorted_txids[0][..8].to_vec()],
        };

        let exclude_fetch_service_stream = fetch_service_subscriber
            .get_mempool_tx(exclude_list.clone())
            .await
            .unwrap();
        let exclude_fetch_service_mempool_tx: Vec<_> = exclude_fetch_service_stream.collect().await;

        let exclude_fetch_mempool_tx: Vec<_> = exclude_fetch_service_mempool_tx
            .into_iter()
            .filter_map(|result| result.ok())
            .collect();

        let mut sorted_exclude_fetch_mempool_tx = exclude_fetch_mempool_tx.clone();
        sorted_exclude_fetch_mempool_tx.sort_by_key(|tx| tx.hash.clone());

        assert_eq!(sorted_exclude_fetch_mempool_tx[0].hash, sorted_txids[1]);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_mempool_stream_zcashd() {
        fetch_service_get_mempool_stream("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_mempool_stream_zebrad() {
        fetch_service_get_mempool_stream("zebrad").await;
    }

    async fn fetch_service_get_mempool_stream(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");

        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let fetch_service_handle = tokio::spawn(async move {
            let fetch_service_stream = fetch_service_subscriber.get_mempool_stream().await.unwrap();
            let fetch_service_mempool_tx: Vec<_> = fetch_service_stream.collect().await;
            fetch_service_mempool_tx
                .into_iter()
                .filter_map(|result| result.ok())
                .collect::<Vec<_>>()
        });

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("transparent").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();
        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(
                &clients.get_recipient_address("unified").await,
                250_000,
                None,
            )],
        )
        .await
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let fetch_mempool_tx = fetch_service_handle.await.unwrap();

        let mut sorted_fetch_mempool_tx = fetch_mempool_tx.clone();
        sorted_fetch_mempool_tx.sort_by_key(|tx| tx.data.clone());

        dbg!(sorted_fetch_mempool_tx);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_tree_state_zcashd() {
        fetch_service_get_tree_state("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_tree_state_zebrad() {
        fetch_service_get_tree_state("zebrad").await;
    }

    async fn fetch_service_get_tree_state(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let block_id = BlockId {
            height: 1,
            hash: Vec::new(),
        };

        let fetch_service_get_tree_state = dbg!(fetch_service_subscriber
            .get_tree_state(block_id.clone())
            .await
            .unwrap());

        dbg!(fetch_service_get_tree_state);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_latest_tree_state_zcashd() {
        fetch_service_get_latest_tree_state("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_latest_tree_state_zebrad() {
        fetch_service_get_latest_tree_state("zebrad").await;
    }

    async fn fetch_service_get_latest_tree_state(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        dbg!(fetch_service_subscriber
            .get_latest_tree_state()
            .await
            .unwrap());

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_subtree_roots_zcashd() {
        fetch_service_get_subtree_roots("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_subtree_roots_zebrad() {
        fetch_service_get_subtree_roots("zebrad").await;
    }

    async fn fetch_service_get_subtree_roots(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let subtree_roots_arg = GetSubtreeRootsArg {
            start_index: 0,
            shielded_protocol: 1,
            max_entries: 0,
        };

        let fetch_service_stream = fetch_service_subscriber
            .get_subtree_roots(subtree_roots_arg.clone())
            .await
            .unwrap();
        let fetch_service_roots: Vec<_> = fetch_service_stream.collect().await;

        let fetch_roots: Vec<_> = fetch_service_roots
            .into_iter()
            .filter_map(|result| result.ok())
            .collect();

        dbg!(fetch_roots);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_taddress_utxos_zcashd() {
        fetch_service_get_taddress_utxos("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_taddress_utxos_zebrad() {
        fetch_service_get_taddress_utxos("zebrad").await;
    }

    async fn fetch_service_get_taddress_utxos(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");
        let recipient_address = clients.get_recipient_address("transparent").await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        let tx = zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(&recipient_address, 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let utxos_arg = GetAddressUtxosArg {
            addresses: vec![recipient_address],
            start_height: 0,
            max_entries: 0,
        };

        let fetch_service_get_taddress_utxos = fetch_service_subscriber
            .get_address_utxos(utxos_arg.clone())
            .await
            .unwrap();

        dbg!(tx);
        dbg!(&fetch_service_get_taddress_utxos);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_taddress_utxos_stream_zcashd() {
        fetch_service_get_taddress_utxos_stream("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_taddress_utxos_stream_zebrad() {
        fetch_service_get_taddress_utxos_stream("zebrad").await;
    }

    async fn fetch_service_get_taddress_utxos_stream(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        let clients = test_manager
            .clients
            .as_ref()
            .expect("Clients are not initialized");
        let recipient_address = clients.get_recipient_address("transparent").await;

        clients.faucet.do_sync(true).await.unwrap();

        if validator == "zebrad" {
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.do_sync(true).await.unwrap();
        };

        zingolib::testutils::lightclient::from_inputs::quick_send(
            &clients.faucet,
            vec![(&recipient_address, 250_000, None)],
        )
        .await
        .unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let utxos_arg = GetAddressUtxosArg {
            addresses: vec![recipient_address],
            start_height: 0,
            max_entries: 0,
        };

        let fetch_service_stream = fetch_service_subscriber
            .get_address_utxos_stream(utxos_arg.clone())
            .await
            .unwrap();
        let fetch_service_utxos: Vec<_> = fetch_service_stream.collect().await;

        let fetch_utxos: Vec<_> = fetch_service_utxos
            .into_iter()
            .filter_map(|result| result.ok())
            .collect();

        dbg!(fetch_utxos);

        test_manager.close().await;
    }

    #[tokio::test]
    async fn fetch_service_get_lightd_info_zcashd() {
        fetch_service_get_lightd_info("zcashd").await;
    }

    #[tokio::test]
    async fn fetch_service_get_lightd_info_zebrad() {
        fetch_service_get_lightd_info("zebrad").await;
    }

    async fn fetch_service_get_lightd_info(validator: &str) {
        let (mut test_manager, _fetch_service, fetch_service_subscriber) =
            create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

        dbg!(fetch_service_subscriber.get_lightd_info().await.unwrap());

        test_manager.close().await;
    }
}
