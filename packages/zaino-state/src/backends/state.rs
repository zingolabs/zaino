//! Zcash chain fetch and tx submission service backed by Zebras [`ReadStateService`].

#[allow(deprecated)]
use crate::{
    chain_index::{
        chain_tips_from_nonfinalized_snapshot, source::ValidatorConnector, types as chain_types,
        ChainIndex, ChainIndexRpcExt,
    },
    config::{DonationAddress, StateServiceConfig},
    error::{BlockCacheError, StateServiceError},
    indexer::{
        handle_raw_transaction, IndexerSubscriber, LightWalletIndexer, ZcashIndexer, ZcashService,
    },
    status::{Status, StatusType},
    stream::{
        AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
        UtxoReplyStream,
    },
    utils::{get_build_info, ServiceMetadata},
    BackendType, NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
};
use crate::{
    chain_index::{types::BestChainLocation, NonFinalizedSnapshot},
    TransactionHash,
};
use tokio_stream::StreamExt as _;
use zaino_fetch::{
    chain::{transaction::FullTransaction, utils::ParseFromSlice},
    jsonrpsee::{
        connector::RpcError,
        response::{
            address_deltas::{GetAddressDeltasParams, GetAddressDeltasResponse},
            block_deltas::BlockDeltas,
            block_header::GetBlockHeader,
            block_subsidy::GetBlockSubsidy,
            chain_tips::GetChainTipsResponse,
            mining_info::GetMiningInfoWire,
            peer_info::GetPeerInfo,
            z_validate_address::ZValidateAddressResponse,
            GetMempoolInfoResponse, GetNetworkSolPsResponse, GetSpentInfoRequest,
            GetSpentInfoResponse, GetTxOutResponse, GetTxOutSetInfoResponse,
        },
    },
};
use zaino_proto::proto::utils::{
    blockid_to_hashorheight, compact_block_to_nullifiers, GetBlockRangeError, PoolTypeError,
    PoolTypeFilter, ValidatedBlockRangeRequest,
};
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, Balance, BlockId, BlockRange, GetAddressUtxosArg, GetAddressUtxosReply,
        GetAddressUtxosReplyList, GetMempoolTxRequest, LightdInfo, PingResponse, RawTransaction,
        SendResponse, TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};
use zebra_chain::{
    block::Height, serialization::ZcashDeserialize as _, subtree::NoteCommitmentSubtreeIndex,
};
use zebra_rpc::{
    client::{
        GetAddressBalanceRequest, GetSubtreesByIndexResponse, GetTreestateResponse,
        TransactionObject, ValidateAddressResponse,
    },
    methods::{
        AddressBalance, GetAddressTxIdsRequest, GetAddressUtxos, GetBlock, GetBlockHash,
        GetBlockchainInfoResponse, GetInfo, GetRawTransaction, SentTransactionHash,
    },
    server::error::LegacyCode,
};
use zebra_state::HashOrHeight;

use hex::{FromHex as _, ToHex};
use std::str::FromStr;
use tokio::{
    sync::mpsc,
    time::{self, timeout},
};
use tracing::{info, instrument, warn};

/// Chain fetch service backed by Zebra's `ReadStateService` and `TrustedChainSync`.
///
/// NOTE: We currently dop not implement clone for chain fetch services
/// as this service is responsible for maintaining and closing its child processes.
///       ServiceSubscribers are used to create separate chain fetch processes
/// while allowing central state processes to be managed in a single place.
///       If we want the ability to clone Service all JoinHandle's should be
/// converted to Arc\<JoinHandle\>.
#[derive(Debug)]
// #[deprecated = "Will be eventually replaced by `BlockchainSource"]
pub struct StateService {
    /// Core indexer.
    indexer: NodeBackedChainIndex,

    /// Service metadata.
    data: ServiceMetadata,

    /// StateService config data.
    #[allow(deprecated)]
    config: StateServiceConfig,
}

impl Status for StateService {
    fn status(&self) -> StatusType {
        self.indexer.status()
    }
}

// #[allow(deprecated)]
impl ZcashService for StateService {
    const BACKEND_TYPE: BackendType = BackendType::State;

    type Subscriber = StateServiceSubscriber;
    type Config = StateServiceConfig;

    /// Initializes a new StateService instance and starts sync process.
    #[instrument(name = "StateService::spawn", skip(config), fields(network = %config.common.network))]
    async fn spawn(config: StateServiceConfig) -> Result<Self, StateServiceError> {
        info!(
            rpc_address = %config.common.validator_rpc_address,
            network = %config.common.network,
            "Spawning State Service"
        );

        let (source, zebra_build_data) = ValidatorConnector::spawn_state(&config)
            .await
            .map_err(|error| StateServiceError::Critical(error.to_string()))?;

        let data = ServiceMetadata::new(
            get_build_info(config.common.indexer_version.clone()),
            config.common.network.to_zebra_network(),
            zebra_build_data.build,
            zebra_build_data.subversion,
        );
        info!(build = %data.zebra_build(), subversion = %data.zebra_subversion(), "Connected to Zcash node");

        let indexer = NodeBackedChainIndex::new(source, config.clone().into())
            .await
            .map_err(|error| StateServiceError::Critical(error.to_string()))?;

        let state_service = Self {
            indexer,
            data,
            config,
        };

        // wait for sync to complete, return error on sync fail.
        loop {
            match state_service.status() {
                StatusType::Ready | StatusType::Closing => break,
                StatusType::CriticalError => {
                    return Err(StateServiceError::Critical(
                        "Chain index sync failed".to_string(),
                    ));
                }
                _ => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        }

        Ok(state_service)
    }

    fn get_subscriber(&self) -> IndexerSubscriber<StateServiceSubscriber> {
        IndexerSubscriber::new(StateServiceSubscriber {
            indexer: self.indexer.subscriber(),
            data: self.data.clone(),
            config: self.config.clone(),
        })
    }

    /// Shuts down the StateService.
    ///
    /// Delegates to the indexer, which cancels its sync loop, tears down the
    /// finalised DB and mempool, and aborts the source-owned Zebra chain-syncer
    /// task via [`crate::chain_index::source::BlockchainSource::shutdown`].
    fn close(&mut self) {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let _ = self.indexer.shutdown().await;
            });
        });
    }
}

#[allow(deprecated)]
impl Drop for StateService {
    fn drop(&mut self) {
        self.close()
    }
}

/// A fetch service subscriber.
///
/// Subscribers should be
#[derive(Debug, Clone)]
// #[deprecated]
pub struct StateServiceSubscriber {
    /// Core indexer.
    pub indexer: NodeBackedChainIndexSubscriber,

    /// Service metadata.
    pub data: ServiceMetadata,

    /// StateService config data.
    #[allow(deprecated)]
    config: StateServiceConfig,
}

impl StateServiceSubscriber {
    /// The backing Zebra [`ReadStateService`].
    ///
    /// Test-only escape hatch: live tests recompute expected chain data (e.g.
    /// treestate roots) directly off the `ReadStateService`. Production code goes
    /// through the `ChainIndex` API.
    #[cfg(feature = "test_dependencies")]
    pub fn read_state_service(&self) -> zebra_state::ReadStateService {
        self.indexer
            .source()
            .read_state_service()
            .expect("StateServiceSubscriber is always State-backed")
            .clone()
    }

    /// The indexer's mempool subscriber.
    ///
    /// Test-only escape hatch: live tests recompute expected `getmempoolinfo`
    /// values directly off the mempool's entries. Production code goes through the
    /// `ChainIndex` mempool API.
    #[cfg(feature = "test_dependencies")]
    pub fn mempool(&self) -> &crate::chain_index::mempool::MempoolSubscriber {
        self.indexer.mempool_subscriber()
    }
}

impl Status for StateServiceSubscriber {
    fn status(&self) -> StatusType {
        self.indexer.status()
    }
}

/// A subscriber to any chaintip updates
#[derive(Clone)]
pub struct ChainTipSubscriber {
    monitor: zebra_state::ChainTipChange,
}

impl ChainTipSubscriber {
    /// Waits until the tip hash has changed (relative to the last time this method
    /// was called), then returns the best tip's block hash.
    pub async fn next_tip_hash(
        &mut self,
    ) -> Result<zebra_chain::block::Hash, tokio::sync::watch::error::RecvError> {
        self.monitor
            .wait_for_tip_change()
            .await
            .map(|tip| tip.best_tip_hash())
    }
}

/// Private RPC methods, which are used as helper methods by the public ones
///
/// These would be simple to add to the public interface if
/// needed, there are currently no plans to do so.
// #[allow(deprecated)]
impl StateServiceSubscriber {
    /// Gets a Subscriber to any updates to the latest chain tip
    pub fn chaintip_update_subscriber(&self) -> ChainTipSubscriber {
        ChainTipSubscriber {
            monitor: self
                .indexer
                .source()
                .chain_tip_change()
                .expect("StateServiceSubscriber is always State-backed"),
        }
    }
    /// Return a list of consecutive compact blocks.
    #[allow(dead_code, deprecated)]
    async fn get_block_range_inner(
        &self,
        request: BlockRange,
        nullifiers_only: bool,
    ) -> Result<CompactBlockStream, StateServiceError> {
        let validated_request = ValidatedBlockRangeRequest::new_from_block_range(&request)
            .map_err(StateServiceError::from)?;

        let pool_type_filter = PoolTypeFilter::new_from_pool_types(&validated_request.pool_types())
            .map_err(GetBlockRangeError::PoolTypeArgumentError)
            .map_err(StateServiceError::from)?;

        // Note conversion here is safe due to the use of [`ValidatedBlockRangeRequest::new_from_block_range`]
        let start = validated_request.start() as u32;
        let end = validated_request.end() as u32;

        let state_service_clone = self.clone();
        let service_timeout = self.config.common.service.timeout;
        let (channel_tx, channel_rx) =
            mpsc::channel(self.config.common.service.channel_size as usize);
        let snapshot = state_service_clone
            .indexer
            .snapshot_nonfinalized_state()
            .await?;

        tokio::spawn(async move {
            let timeout_result = timeout(
            time::Duration::from_secs((service_timeout * 4) as u64),
            async {
                // This method does not support passthrough. Just return.
                let Some(non_finalized_snapshot) = snapshot.get_nfs_snapshot() else {return};
                let chain_height = non_finalized_snapshot.best_tip.height.0;

                match state_service_clone
                    .indexer
                    .get_compact_block_stream(
                        &snapshot,
                        chain_types::Height(start),
                        chain_types::Height(end),
                        pool_type_filter.clone(),
                    )
                    .await
                {
                    Ok(Some(mut compact_block_stream)) => {
                        if nullifiers_only {
                            while let Some(stream_item) = compact_block_stream.next().await {
                                match stream_item {
                                    Ok(block) => {
                                        if channel_tx
                                            .send(Ok(compact_block_to_nullifiers(block)))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    Err(status) => {
                                        if channel_tx.send(Err(status)).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                            }
                        } else {
                            while let Some(stream_item) = compact_block_stream.next().await {
                                if channel_tx.send(stream_item).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        // Per `get_compact_block_stream` semantics: `None` means at least one bound is above the tip.
                        let offending_height = if start > chain_height { start } else { end };

                        match channel_tx
                            .send(Err(tonic::Status::out_of_range(format!(
                                "Error: Height out of range [{offending_height}]. Height requested is greater than the best chain tip [{chain_height}].",
                            ))))
                            .await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                warn!(%e, "GetBlockRange channel closed unexpectedly");
                            }
                        }
                    }
                    Err(e) => {
                        // Preserve previous behaviour: if the request is above tip, surface OutOfRange;
                        // otherwise return the error (currently exposed for dev).
                        if start > chain_height || end > chain_height {
                            let offending_height = if start > chain_height { start } else { end };

                            match channel_tx
                                .send(Err(tonic::Status::out_of_range(format!(
                                    "Error: Height out of range [{offending_height}]. Height requested is greater than the best chain tip [{chain_height}].",
                                ))))
                                .await
                            {
                                Ok(_) => {}
                                Err(e) => {
                                    warn!(%e, "GetBlockRange channel closed unexpectedly");
                                }
                            }
                        } else {
                            // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                            if channel_tx
                                .send(Err(tonic::Status::unknown(e.to_string())))
                                .await
                                .is_err()
                            {
                                warn!(%e, "GetBlockRangeStream closed unexpectedly");
                            }
                        }
                    }
                }
            },
        )
        .await;

            if timeout_result.is_err() {
                channel_tx
                    .send(Err(tonic::Status::deadline_exceeded(
                        "Error: get_block_range gRPC request timed out.",
                    )))
                    .await
                    .ok();
            }
        });

        Ok(CompactBlockStream::new(channel_rx))
    }

    async fn error_get_block(
        &self,
        e: BlockCacheError,
        height: u32,
    ) -> Result<CompactBlock, StateServiceError> {
        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
        let chain_height = snapshot.max_serviceable_height().0;
        Err(if height >= chain_height {
            StateServiceError::TonicStatusError(tonic::Status::out_of_range(format!(
                "Error: Height out of range [{height}]. Height requested \
                                is greater than Zaino's best chain tip [{chain_height}].",
            )))
        } else {
            // TODO: Hide server error from clients before release.
            // Currently useful for dev purposes.
            StateServiceError::TonicStatusError(tonic::Status::unknown(format!(
                "Error: Failed to retrieve block from node. Server Error: {e}",
            )))
        })
    }

    /// Returns the network type running.
    #[allow(deprecated)]
    pub fn network(&self) -> zaino_common::Network {
        self.config.common.network
    }
}

// #[allow(deprecated)]
impl ZcashIndexer for StateServiceSubscriber {
    type Error = StateServiceError;

    async fn get_info(&self) -> Result<GetInfo, Self::Error> {
        Ok(self.indexer.get_info().await?)
    }

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
    ) -> Result<GetAddressDeltasResponse, Self::Error> {
        Ok(self.indexer.get_address_deltas(params).await?)
    }

    async fn get_difficulty(&self) -> Result<f64, Self::Error> {
        Ok(self.indexer.get_difficulty().await?)
    }

    async fn get_block_subsidy(&self, height: u32) -> Result<GetBlockSubsidy, Self::Error> {
        Ok(self.indexer.get_block_subsidy(height).await?)
    }

    async fn get_blockchain_info(&self) -> Result<GetBlockchainInfoResponse, Self::Error> {
        Ok(self.indexer.get_blockchain_info().await?)
    }

    /// Returns details on the active state of the TX memory pool.
    /// In Zaino, this RPC call information is gathered from the local Zaino state instead of directly reflecting the full node's mempool. This state is populated from a gRPC stream, sourced from the full node.
    /// There are no request parameters.
    /// The Zcash source code is considered canonical:
    /// [from the rpc definition](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1555>), [this function is called to produce the return value](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1541>>).
    /// There are no required or optional parameters.
    /// the `size` field is called by [this line of code](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1544>), and returns an int64.
    /// `size` represents the number of transactions currently in the mempool.
    /// the `bytes` field is called by [this line of code](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1545>), and returns an int64 from [this variable](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/txmempool.h#L349>).
    /// `bytes` is the sum memory size in bytes of all transactions in the mempool: the sum of all transaction byte sizes.
    /// the `usage` field is called by [this line of code](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1546>), and returns an int64 derived from the return of this function(<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/txmempool.h#L1199>), which includes a number of elements.
    /// `usage` is the total memory usage for the mempool, in bytes.
    /// the [optional `fullyNotified` field](<https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L1549>), is only utilized for zcashd regtests, is deprecated, and is not included.
    async fn get_mempool_info(&self) -> Result<GetMempoolInfoResponse, Self::Error> {
        Ok(self.indexer.get_mempool_info().await.into())
    }

    async fn get_peer_info(&self) -> Result<GetPeerInfo, Self::Error> {
        Ok(self.indexer.get_peer_info().await?)
    }

    async fn z_get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<AddressBalance, Self::Error> {
        Ok(self.indexer.get_address_balance(address_strings).await?)
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<SentTransactionHash, Self::Error> {
        Ok(self
            .indexer
            .send_raw_transaction(raw_transaction_hex)
            .await?)
    }

    async fn get_block_header(
        &self,
        hash: String,
        verbose: bool,
    ) -> Result<GetBlockHeader, Self::Error> {
        Ok(self.indexer.get_block_header(hash, verbose).await?)
    }

    async fn z_get_block(
        &self,
        hash_or_height_string: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        Ok(self
            .indexer
            .z_get_block(hash_or_height_string, verbosity)
            .await?)
    }

    async fn get_block_deltas(&self, hash: String) -> Result<BlockDeltas, Self::Error> {
        Ok(self.indexer.get_block_deltas(hash).await?)
    }

    async fn get_raw_mempool(&self) -> Result<Vec<String>, Self::Error> {
        Ok(self
            .indexer
            .get_mempool_txids()
            .await?
            .into_iter()
            .map(|txid| txid.to_rpc_hex())
            .collect())
    }

    /// NOTE: This method currently has to fetch data from 2 places (get_treestate and get_indexed_block_by_*),
    ///       If `ValidatorConnector::GetTreeState` was updated to return the additional information
    ///       required, this second call could be removed, improving the performance of this method.
    // Pre-existing lint: `StateServiceError` is a large error type; returning it by value here is
    // flagged by `result_large_err`. Suppressed to satisfy `-D warnings` without an invasive
    // boxing refactor of the shared error enum.
    #[allow(clippy::result_large_err)]
    async fn z_get_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<GetTreestateResponse, Self::Error> {
        let fallback_hash_or_height = hash_or_height.clone();
        let local_result: Result<GetTreestateResponse, Self::Error> = async {
            let hash_or_height_struct: HashOrHeight = HashOrHeight::from_str(&hash_or_height)?;
            let snapshot = self.indexer.snapshot_nonfinalized_state().await?;

            let block_data = match hash_or_height_struct {
                HashOrHeight::Hash(hash) => self
                    .indexer
                    .get_indexed_block_by_hash(&snapshot, &hash.into())
                    .await?
                    .ok_or(StateServiceError::RpcError(RpcError::new_from_legacycode(
                        zebra_rpc::server::error::LegacyCode::InvalidParameter,
                        "Failed to fetch block data.",
                    )))?,
                HashOrHeight::Height(height) => self
                    .indexer
                    .get_indexed_block_by_height(&snapshot, &height.into())
                    .await?
                    .ok_or(StateServiceError::RpcError(RpcError::new_from_legacycode(
                        zebra_rpc::server::error::LegacyCode::InvalidParameter,
                        "Failed to fetch block data.",
                    )))?,
            };

            let (sapling, orchard) = self.indexer.get_treestate(block_data.hash()).await?;
            let time: u32 = block_data.data().time().try_into().map_err(|_error| {
                StateServiceError::RpcError(RpcError::new_from_legacycode(
                    zebra_rpc::server::error::LegacyCode::InvalidParameter,
                    "Block time is out of range for u32.",
                ))
            })?;

            #[allow(deprecated)]
            Ok(GetTreestateResponse::from_parts(
                (*block_data.hash()).into(),
                block_data.height().into(),
                time,
                sapling,
                orchard,
            ))
        }
        .await;

        if let Ok(response) = local_result {
            return Ok(response);
        }

        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
        if !self
            .indexer
            .hash_or_height_known_for_treestate(&snapshot, &fallback_hash_or_height)
            .await?
        {
            return local_result;
        }

        Ok(self
            .indexer
            .get_treestate_by_id(fallback_hash_or_height)
            .await?)
    }

    async fn get_mining_info(&self) -> Result<GetMiningInfoWire, Self::Error> {
        Ok(self.indexer.get_mining_info().await?)
    }

    /// Returns statistics about the unspent transaction output set.
    ///
    /// zcashd reference: [`gettxoutsetinfo`](https://zcash.github.io/rpc/gettxoutsetinfo.html)
    /// method: post
    /// tags: blockchain
    async fn get_tx_out_set_info(&self) -> Result<GetTxOutSetInfoResponse, Self::Error> {
        Ok(self.indexer.get_tx_out_set_info().await?)
    }

    // No request parameters.
    /// Return the hex encoded hash of the best (tip) block, in the longest block chain.
    /// The Zcash source code is considered canonical:
    /// [In the rpc definition](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/common.h#L48) there are no required params, or optional params.
    /// [The function in rpc/blockchain.cpp](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L325)
    /// where `return chainActive.Tip()->GetBlockHash().GetHex();` is the [return expression](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L339)returning a `std::string`
    async fn get_best_blockhash(&self) -> Result<GetBlockHash, Self::Error> {
        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
        let tip = self.indexer.best_chaintip(&snapshot).await?;
        Ok(GetBlockHash::new(tip.hash.into()))
    }

    /// Returns the current block count in the best valid block chain.
    ///
    /// zcashd reference: [`getblockcount`](https://zcash.github.io/rpc/getblockcount.html)
    /// method: post
    /// tags: blockchain
    async fn get_block_count(&self) -> Result<Height, Self::Error> {
        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
        let tip = self.indexer.best_chaintip(&snapshot).await?;
        Ok(tip.height.into())
    }

    async fn get_chain_tips(&self) -> Result<GetChainTipsResponse, Self::Error> {
        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
        let Some(non_finalized_snapshot) = snapshot.get_nfs_snapshot() else {
            return Err(StateServiceError::UnavailableNotSyncedEnough);
        };
        Ok(chain_tips_from_nonfinalized_snapshot(
            non_finalized_snapshot,
        ))
    }

    async fn validate_address(
        &self,
        raw_address: String,
    ) -> Result<ValidateAddressResponse, Self::Error> {
        let network = self.config.common.network.to_zebra_network();
        Ok(crate::indexer::validate_address(raw_address, &network))
    }

    #[allow(deprecated)]
    async fn z_validate_address(
        &self,
        address: String,
    ) -> Result<ZValidateAddressResponse, Self::Error> {
        let network = self.config.common.network.to_zebra_network();
        Ok(crate::indexer::z_validate_address(address, &network))
    }

    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<GetSubtreesByIndexResponse, Self::Error> {
        let shielded_pool = match pool.as_str() {
            "sapling" => crate::chain_index::ShieldedPool::Sapling,
            "orchard" => crate::chain_index::ShieldedPool::Orchard,
            otherwise => {
                return Err(StateServiceError::RpcError(RpcError::new_from_legacycode(
                    LegacyCode::Misc,
                    format!(
                        "invalid pool name \"{otherwise}\", must be \"sapling\" or \"orchard\""
                    ),
                )))
            }
        };
        let roots = self
            .indexer
            .get_subtree_roots(shielded_pool, start_index.0, limit.map(|index| index.0))
            .await?;
        Ok(crate::indexer::build_subtrees_by_index_response(
            pool,
            start_index,
            roots,
        ))
    }

    async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetRawTransaction, Self::Error> {
        #[allow(deprecated)]
        let txid = TransactionHash::from_hex(&txid_hex).map_err(|error| {
            StateServiceError::RpcError(RpcError::new_from_legacycode(
                zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey,
                error.to_string(),
            ))
        })?;

        #[allow(deprecated)]
        let not_found_error = || {
            StateServiceError::RpcError(RpcError::new_from_legacycode(
                zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey,
                "No such mempool or main chain transaction",
            ))
        };

        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;

        let Some((serialized_transaction, _consensus_branch_id)) =
            self.indexer.get_raw_transaction(&snapshot, &txid).await?
        else {
            return Err(not_found_error());
        };

        if verbose.is_none() {
            return Ok(GetRawTransaction::Raw(
                zebra_chain::transaction::SerializedTransaction::from(serialized_transaction),
            ));
        }

        let transaction = zebra_chain::transaction::Transaction::zcash_deserialize(
            serialized_transaction.as_slice(),
        )
        .map_err(|_| not_found_error())?;

        let (best_chain_location, _non_best_chain_locations) = self
            .indexer
            .get_transaction_status(&snapshot, &txid)
            .await?;

        let (height, confirmations, block_hash, in_best_chain) = match best_chain_location {
            Some(BestChainLocation::Block(block_hash, height)) => {
                let confirmations = snapshot
                    .max_serviceable_height()
                    .0
                    .saturating_sub(height.0)
                    .saturating_add(1);

                (
                    Some(zebra_chain::block::Height::from(height)),
                    Some(confirmations),
                    Some(zebra_chain::block::Hash::from(block_hash)),
                    Some(true),
                )
            }
            Some(BestChainLocation::Mempool(_height)) => (None, Some(0), None, Some(false)),
            None => (None, None, None, Some(false)),
        };

        Ok(GetRawTransaction::Object(Box::new(
            TransactionObject::from_transaction(
                transaction.into(),
                height,
                confirmations,
                #[allow(deprecated)]
                &self.config.common.network.to_zebra_network(),
                None,
                block_hash,
                in_best_chain,
                zebra_chain::transaction::Hash::from(txid),
            ),
        )))
    }

    /// Returns details about an unspent transaction output.
    ///
    /// zcashd reference: [`gettxout`](https://zcash.github.io/rpc/gettxout.html)
    /// method: post
    /// tags: transaction
    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> Result<GetTxOutResponse, Self::Error> {
        Ok(self.indexer.get_tx_out(txid, n, include_mempool).await?)
    }

    async fn get_spent_info(
        &self,
        request: GetSpentInfoRequest,
    ) -> Result<GetSpentInfoResponse, Self::Error> {
        Ok(self.indexer.get_spent_info(request).await?)
    }

    async fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, Self::Error> {
        Ok(self
            .indexer
            .get_address_txids(request)
            .await?
            .into_iter()
            .map(|transaction_hash| transaction_hash.to_rpc_hex())
            .collect())
    }

    async fn z_get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<Vec<GetAddressUtxos>, Self::Error> {
        Ok(self.indexer.get_address_utxos(address_strings).await?)
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
        Ok(self.indexer.get_network_sol_ps(blocks, height).await?)
    }

    // Helper function, to get the chain height in rpc implementations
    async fn chain_height(&self) -> Result<Height, Self::Error> {
        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
        Ok(self.indexer.best_chaintip(&snapshot).await?.height.into())
    }
}

// #[allow(deprecated)]
impl LightWalletIndexer for StateServiceSubscriber {
    /// Return the height of the tip of the best chain
    async fn get_latest_block(&self) -> Result<BlockId, Self::Error> {
        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
        let Some(non_finalized_snapshot) = snapshot.get_nfs_snapshot() else {
            // TODO: This probably shouldn't be an error.
            // this is an improvement over previous behaviour of
            // acting as if we are only synced to the genesis block
            return Err(StateServiceError::UnavailableNotSyncedEnough);
        };
        Ok(non_finalized_snapshot.best_tip.to_wire())
    }

    /// Return the compact block corresponding to the given block identifier
    async fn get_block(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        let hash_or_height = blockid_to_hashorheight(request).ok_or(
            StateServiceError::TonicStatusError(tonic::Status::invalid_argument(
                "Error: Invalid hash and/or height out of range. Failed to convert to u32.",
            )),
        )?;

        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;

        // Convert HashOrHeight to chain_types::Height
        let block_height = match hash_or_height {
            HashOrHeight::Height(h) => chain_types::Height(h.0),
            HashOrHeight::Hash(h) => self
                .indexer
                .get_block_height(&snapshot, chain_types::BlockHash(h.0))
                .await
                .map_err(StateServiceError::ChainIndexError)?
                .ok_or_else(|| {
                    StateServiceError::TonicStatusError(tonic::Status::not_found(
                        "Error: Block not found for given hash.",
                    ))
                })?,
        };

        match self
            .indexer
            .get_compact_block(&snapshot, block_height, PoolTypeFilter::includes_all())
            .await
        {
            Ok(Some(block)) => Ok(block),
            Ok(None) => {
                let Some(non_finalized_snapshot) = snapshot.get_nfs_snapshot() else {
                    // TODO: This probably shouldn't be an error.
                    // this is an improvement over previous behaviour of
                    // acting as if we are only synced to the genesis block
                    return Err(StateServiceError::UnavailableNotSyncedEnough);
                };
                let chain_height = non_finalized_snapshot.best_tip.height.0;
                match hash_or_height {
                    HashOrHeight::Height(Height(height)) if height >= chain_height => Err(
                        StateServiceError::TonicStatusError(tonic::Status::out_of_range(format!(
                            "Error: Height out of range [{hash_or_height}]. Height requested \
                                is greater than the best chain tip [{chain_height}].",
                        ))),
                    ),
                    _otherwise => Err(StateServiceError::TonicStatusError(tonic::Status::unknown(
                        "Error: Failed to retrieve block from state.",
                    ))),
                }
            }
            Err(e) => {
                let Some(non_finalized_snapshot) = snapshot.get_nfs_snapshot() else {
                    // TODO: This probably shouldn't be an error.
                    // this is an improvement over previous behaviour of
                    // acting as if we are only synced to the genesis block
                    return Err(StateServiceError::UnavailableNotSyncedEnough);
                };
                let chain_height = non_finalized_snapshot.best_tip.height.0;
                match hash_or_height {
                    HashOrHeight::Height(Height(height)) if height >= chain_height => Err(
                        StateServiceError::TonicStatusError(tonic::Status::out_of_range(format!(
                            "Error: Height out of range [{hash_or_height}]. Height requested \
                                is greater than the best chain tip [{chain_height}].",
                        ))),
                    ),
                    _otherwise =>
                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                    {
                        Err(StateServiceError::TonicStatusError(tonic::Status::unknown(
                            format!("Error: Failed to retrieve block from node. Server Error: {e}",),
                        )))
                    }
                }
            }
        }
    }

    /// Same as GetBlock except actions contain only nullifiers,
    /// and saling outputs are not returned (Sapling spends still are)
    async fn get_block_nullifiers(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        let height: u32 = request.height.try_into().map_err(|_| {
            StateServiceError::TonicStatusError(tonic::Status::invalid_argument(
                "Error: Height out of range. Failed to convert to u32.",
            ))
        })?;

        let snapshot = self.indexer.snapshot_nonfinalized_state().await?;
        let block_height = chain_types::Height(height);

        match self
            .indexer
            .get_compact_block(&snapshot, block_height, PoolTypeFilter::includes_all())
            .await
        {
            Ok(Some(block)) => Ok(compact_block_to_nullifiers(block)),
            Ok(None) => {
                self.error_get_block(
                    BlockCacheError::Custom("Block not found".to_string()),
                    height,
                )
                .await
            }
            Err(e) => Err(StateServiceError::ChainIndexError(e)),
        }
    }

    /// Return a list of consecutive compact blocks
    async fn get_block_range(
        &self,
        blockrange: BlockRange,
    ) -> Result<CompactBlockStream, StateServiceError> {
        self.get_block_range_inner(blockrange, false).await
    }
    /// Same as GetBlockRange except actions contain only nullifiers
    async fn get_block_range_nullifiers(
        &self,
        request: BlockRange,
    ) -> Result<CompactBlockStream, Self::Error> {
        self.get_block_range_inner(request, true).await
    }

    /// Return the requested full (not compact) transaction (as from zcashd)
    async fn get_transaction(&self, request: TxFilter) -> Result<RawTransaction, Self::Error> {
        let hash = zebra_chain::transaction::Hash::from(
            <[u8; 32]>::try_from(request.hash).map_err(|_| {
                StateServiceError::TonicStatusError(tonic::Status::invalid_argument(
                    "Error: Transaction hash incorrect",
                ))
            })?,
        );
        let hex = hash.encode_hex();

        // explicit over method call syntax to make it clear where this method is coming from
        #[allow(clippy::result_large_err)]
        <Self as ZcashIndexer>::get_raw_transaction(self, hex, Some(1))
            .await
            .and_then(|grt| match grt {
                GetRawTransaction::Raw(_serialized_transaction) => Err(StateServiceError::Custom(
                    "unreachable, verbose transaction expected".to_string(),
                )),
                GetRawTransaction::Object(transaction_object) => Ok(RawTransaction {
                    data: transaction_object.hex().as_ref().to_vec(),
                    height: transaction_object.height().unwrap_or(0) as u64,
                }),
            })
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

    /// Return the transactions corresponding to the given t-address within the given block range
    async fn get_taddress_transactions(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        let chain_height = self.chain_height().await?;
        let txids = self.get_taddress_txids_helper(request).await?;
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.common.service.timeout;
        let (transmitter, receiver) =
            mpsc::channel(self.config.common.service.channel_size as usize);
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

    /// Return the txids corresponding to the given t-address within the given block range
    /// This function is deprecated. Use `get_taddress_transactions`.
    async fn get_taddress_txids(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        self.get_taddress_transactions(request).await
    }

    /// Returns the total balance for a list of taddrs
    async fn get_taddress_balance(
        &self,
        request: AddressList,
    ) -> Result<zaino_proto::proto::service::Balance, Self::Error> {
        let taddrs = GetAddressBalanceRequest::new(request.addresses);
        let balance = self.z_get_address_balance(taddrs).await?;
        let checked_balance: i64 = match i64::try_from(balance.balance()) {
            Ok(balance) => balance,
            Err(_) => {
                return Err(StateServiceError::TonicStatusError(tonic::Status::unknown(
                    "Error: Error converting balance from u64 to i64.",
                )));
            }
        };
        Ok(Balance {
            value_zat: checked_balance,
        })
    }

    /// Returns the total balance for a list of taddrs
    ///
    /// TODO: This is taken from fetch.rs, we could / probably should reconfigure into a trait implementation.
    async fn get_taddress_balance_stream(
        &self,
        mut request: AddressStream,
    ) -> Result<zaino_proto::proto::service::Balance, Self::Error> {
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.common.service.timeout;
        let (channel_tx, mut channel_rx) =
            mpsc::channel::<String>(self.config.common.service.channel_size as usize);
        let fetcher_task_handle = tokio::spawn(async move {
            let fetcher_timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    let mut total_balance: u64 = 0;
                    loop {
                        match channel_rx.recv().await {
                            Some(taddr) => {
                                let taddrs = GetAddressBalanceRequest::new(vec![taddr]);
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
        // NOTE: This timeout is so slow due to the blockcache not
        // being implemented. This should be reduced to 30s once functionality is in place.
        // TODO: Make [rpc_timout] a configurable system variable
        // with [default = 30s] and [mempool_rpc_timout = 4*rpc_timeout]
        let addr_recv_timeout = timeout(
            time::Duration::from_secs((service_timeout * 4) as u64),
            async {
                while let Some(address_result) = request.next().await {
                    // TODO: Hide server error from clients before release.
                    // Currently useful for dev purposes.
                    let address = address_result.map_err(|e| {
                        tonic::Status::unknown(format!("Failed to read from stream: {e}"))
                    })?;
                    if channel_tx.send(address.address).await.is_err() {
                        // TODO: Hide server error from clients before release.
                        // Currently useful for dev purposes.
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
                return Err(StateServiceError::TonicStatusError(e));
            }
            Err(_) => {
                fetcher_task_handle.abort();
                return Err(StateServiceError::TonicStatusError(
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
                        // TODO: Hide server error from clients before release.
                        // Currently useful for dev purposes.
                        return Err(StateServiceError::TonicStatusError(tonic::Status::unknown(
                            "Error: Error converting balance from u64 to i64.",
                        )));
                    }
                };
                Ok(Balance {
                    value_zat: checked_balance,
                })
            }
            Ok(Err(e)) => Err(StateServiceError::TonicStatusError(e)),
            // TODO: Hide server error from clients before release.
            // Currently useful for dev purposes.
            Err(e) => Err(StateServiceError::TonicStatusError(tonic::Status::unknown(
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
    async fn get_mempool_tx(
        &self,
        request: GetMempoolTxRequest,
    ) -> Result<CompactTransactionStream, Self::Error> {
        let mut exclude_txids: Vec<String> = vec![];

        for (i, excluded_id) in request.exclude_txid_suffixes.iter().enumerate() {
            if excluded_id.len() > 32 {
                return Err(StateServiceError::TonicStatusError(
                    tonic::Status::invalid_argument(format!(
                        "Error: excluded txid {} is larger than 32 bytes",
                        i
                    )),
                ));
            }

            // NOTE: the TransactionHash methods cannot be used for this hex encoding as exclusions could be truncated to less than 32 bytes
            let reversed_txid_bytes: Vec<u8> = excluded_id.iter().cloned().rev().collect();
            let hex_string_txid: String = hex::encode(&reversed_txid_bytes);
            exclude_txids.push(hex_string_txid);
        }

        let pool_types = match PoolTypeFilter::new_from_slice(&request.pool_types) {
            Ok(pool_type_filter) => pool_type_filter,
            Err(PoolTypeError::InvalidPoolType) => {
                return Err(StateServiceError::TonicStatusError(
                    tonic::Status::invalid_argument(
                        "Error: An invalid `PoolType' was found".to_string(),
                    ),
                ))
            }
            Err(PoolTypeError::UnknownPoolType(unknown_pool_type)) => {
                return Err(StateServiceError::TonicStatusError(
                    tonic::Status::invalid_argument(format!(
                        "Error: Unknown `PoolType' {} was found",
                        unknown_pool_type
                    )),
                ))
            }
        };

        let indexer = self.indexer.clone();
        let service_timeout = self.config.common.service.timeout;
        let (channel_tx, channel_rx) =
            mpsc::channel(self.config.common.service.channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    let transactions = match indexer.get_mempool_transactions(exclude_txids).await {
                        Ok(transactions) => transactions,
                        Err(e) => {
                            channel_tx
                                .send(Err(tonic::Status::unknown(e.to_string())))
                                .await
                                .ok();
                            return;
                        }
                    };
                    for serialized_transaction_bytes in transactions {
                        let txid = match zebra_chain::transaction::Transaction::zcash_deserialize(
                            &mut std::io::Cursor::new(&serialized_transaction_bytes),
                        ) {
                            Ok(transaction) => transaction.hash().0.to_vec(),
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
                            &serialized_transaction_bytes,
                            Some(vec![txid]),
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
                                                .to_compact_tx(None, &pool_types)
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
    async fn get_mempool_stream(&self) -> Result<RawTransactionStream, Self::Error> {
        let indexer = self.indexer.clone();
        let service_timeout = self.config.common.service.timeout;
        let (channel_tx, channel_rx) =
            mpsc::channel(self.config.common.service.channel_size as usize);
        let snapshot = indexer.snapshot_nonfinalized_state().await?;
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 6) as u64),
                async {
                    let Some(non_finalized_snapshot) = snapshot.get_nfs_snapshot() else {
                        // TODO: This probably shouldn't be an error.
                        // this is an improvement over previous behaviour of
                        // acting as if we are only synced to the genesis block
                        if let Err(e) = channel_tx
                            .send(Err(tonic::Status::failed_precondition(
                                "zaino not yet synced".to_string(),
                            )))
                            .await
                        {
                            warn!(%e, "GetMempoolStream channel closed unexpectedly");
                        };
                        return;
                    };
                    let mempool_height = non_finalized_snapshot.best_tip.height.0;
                    match indexer.get_mempool_stream(None) {
                        Some(mut mempool_stream) => {
                            while let Some(result) = mempool_stream.next().await {
                                match result {
                                    Ok(transaction_bytes) => {
                                        if channel_tx
                                            .send(Ok(RawTransaction {
                                                data: transaction_bytes,
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
                        }
                        None => {
                            warn!("Error fetching stream from mempool, Incorrect chain tip!");
                            channel_tx
                                .send(Err(tonic::Status::internal("Error getting mempool stream")))
                                .await
                                .ok();
                        }
                    };
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
        let hash_or_height = blockid_to_hashorheight(request).ok_or(
            crate::error::StateServiceError::TonicStatusError(tonic::Status::invalid_argument(
                "Invalid hash or height",
            )),
        )?;
        #[allow(deprecated)]
        let (hash, height, time, sapling, orchard) =
            <StateServiceSubscriber as ZcashIndexer>::z_get_treestate(
                self,
                hash_or_height.to_string(),
            )
            .await?
            .into_parts();
        Ok(TreeState {
            network: self
                .config
                .common
                .network
                .to_zebra_network()
                .bip70_network_name(),
            height: height.0 as u64,
            hash: hash.to_string(),
            time,
            sapling_tree: sapling.map(hex::encode).unwrap_or_default(),
            orchard_tree: orchard.map(hex::encode).unwrap_or_default(),
        })
    }

    /// GetLatestTreeState returns the note commitment tree state corresponding to the chain tip.
    async fn get_latest_tree_state(&self) -> Result<TreeState, Self::Error> {
        let latest_block = self.chain_height().await?;
        self.get_tree_state(BlockId {
            height: latest_block.0 as u64,
            hash: vec![],
        })
        .await
    }

    fn timeout_channel_size(&self) -> (u32, u32) {
        (
            self.config.common.service.timeout,
            self.config.common.service.channel_size,
        )
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if
    /// [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are collected and returned as a single Vec.
    async fn get_address_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<GetAddressUtxosReplyList, Self::Error> {
        super::validate_utxo_address_count(request.addresses.len())?;
        let taddrs = GetAddressBalanceRequest::new(request.addresses);
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
                    return Err(StateServiceError::TonicStatusError(tonic::Status::unknown(
                        "Error: Index out of range. Failed to convert to i32.",
                    )));
                }
            };
            let checked_satoshis = match i64::try_from(satoshis) {
                Ok(satoshis) => satoshis,
                Err(_) => {
                    return Err(StateServiceError::TonicStatusError(tonic::Status::unknown(
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
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if
    /// [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are returned in a stream.
    async fn get_address_utxos_stream(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<UtxoReplyStream, Self::Error> {
        super::validate_utxo_address_count(request.addresses.len())?;
        let taddrs = GetAddressBalanceRequest::new(request.addresses);
        let utxos = self.z_get_address_utxos(taddrs).await?;
        let service_timeout = self.config.common.service.timeout;
        let (channel_tx, channel_rx) =
            mpsc::channel(self.config.common.service.channel_size as usize);
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
    ///
    /// TODO: This could be made more efficient by fetching data directly (not using self.get_blockchain_info())
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

        let latest_upgrade = super::latest_network_upgrade(blockchain_info.upgrades())
            .map_err(StateServiceError::TonicStatusError)?
            .into_parts();

        let nu_name = latest_upgrade.0;
        let nu_height = latest_upgrade.1;

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
            donation_address: self
                .config
                .common
                .donation_address
                .as_ref()
                .map(DonationAddress::encode)
                .unwrap_or_default(),
            upgrade_name: nu_name.to_string(),
            upgrade_height: nu_height.0 as u64,
            lightwallet_protocol_version: "v0.4.0".to_string(),
        })
    }

    /// Testing-only, requires lightwalletd --ping-very-insecure (do not enable in production)
    ///
    /// NOTE: Currently unimplemented in Zaino.
    async fn ping(
        &self,
        _request: zaino_proto::proto::service::Duration,
    ) -> Result<PingResponse, Self::Error> {
        Err(crate::error::StateServiceError::TonicStatusError(
            tonic::Status::unimplemented(
                "Ping not yet implemented. If you require this RPC please open an \
            issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git).",
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
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
