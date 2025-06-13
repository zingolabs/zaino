//! Zcash chain fetch and tx submission service backed by Zebras [`ReadStateService`].

use crate::{
    config::StateServiceConfig,
    error::{BlockCacheError, StateServiceError},
    indexer::{
        handle_raw_transaction, IndexerSubscriber, LightWalletIndexer, ZcashIndexer, ZcashService,
    },
    local_cache::{
        compact_block_to_nullifiers,
        mempool::{Mempool, MempoolSubscriber},
        BlockCache, BlockCacheSubscriber,
    },
    status::{AtomicStatus, StatusType},
    stream::{
        AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
        UtxoReplyStream,
    },
    utils::{blockid_to_hashorheight, get_build_info, ServiceMetadata},
    MempoolKey,
};

use nonempty::NonEmpty;
use tokio_stream::StreamExt as _;
use zaino_fetch::{
    chain::{transaction::FullTransaction, utils::ParseFromSlice},
    jsonrpsee::connector::{JsonRpSeeConnector, RpcError},
};
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, Balance, BlockId, BlockRange, Exclude, GetAddressUtxosArg,
        GetAddressUtxosReply, GetAddressUtxosReplyList, LightdInfo, PingResponse, RawTransaction,
        SendResponse, TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};

use zebra_chain::{
    block::{Header, Height, SerializedBlock},
    chain_tip::NetworkChainTipHeightEstimator,
    parameters::{ConsensusBranchId, Network, NetworkUpgrade},
    serialization::ZcashSerialize,
    subtree::NoteCommitmentSubtreeIndex,
};
use zebra_rpc::{
    methods::{
        chain_tip_difficulty,
        hex_data::HexData,
        trees::{GetSubtrees, GetTreestate, SubtreeRpcData},
        types::transaction::TransactionObject,
        AddressBalance, AddressStrings, ConsensusBranchIdHex, GetAddressTxIdsRequest,
        GetAddressUtxos, GetBlock, GetBlockChainInfo, GetBlockHash, GetBlockHeader,
        GetBlockHeaderObject, GetBlockTransaction, GetBlockTrees, GetInfo, GetRawTransaction,
        NetworkUpgradeInfo, NetworkUpgradeStatus, SentTransactionHash, TipConsensusBranch,
    },
    server::error::LegacyCode,
    sync::init_read_state_with_syncer,
};
use zebra_state::{
    HashOrHeight, OutputLocation, ReadRequest, ReadResponse, ReadStateService, TransactionLocation,
};

use chrono::{DateTime, Utc};
use futures::{TryFutureExt as _, TryStreamExt as _};
use hex::{FromHex as _, ToHex};
use indexmap::IndexMap;
use std::{collections::HashSet, future::poll_fn, str::FromStr, sync::Arc};
use tokio::{
    sync::mpsc,
    time::{self, timeout},
};
use tonic::async_trait;
use tower::{Service, ServiceExt};
use tracing::{info, warn};

macro_rules! expected_read_response {
    ($response:ident, $expected_variant:ident) => {
        match $response {
            ReadResponse::$expected_variant(inner) => inner,
            unexpected => {
                unreachable!("Unexpected response from state service: {unexpected:?}")
            }
        }
    };
}

/// Chain fetch service backed by Zebra's `ReadStateService` and `TrustedChainSync`.
///
/// NOTE: We currently dop not implement clone for chain fetch services
/// as this service is responsible for maintaining and closing its child processes.
///       ServiceSubscribers are used to create separate chain fetch processes
/// while allowing central state processes to be managed in a single place.
///       If we want the ability to clone Service all JoinHandle's should be
/// converted to Arc<JoinHandle>.
#[derive(Debug)]
pub struct StateService {
    /// `ReadeStateService` from Zebra-State.
    read_state_service: ReadStateService,
    /// Sync task handle.
    sync_task_handle: Option<Arc<tokio::task::JoinHandle<()>>>,
    /// JsonRPC Client.
    rpc_client: JsonRpSeeConnector,
    /// Local compact block cache.
    block_cache: BlockCache,
    /// Internal mempool.
    mempool: Mempool,
    /// Service metadata.
    data: ServiceMetadata,
    /// StateService config data.
    config: StateServiceConfig,
    /// Thread-safe status indicator.
    status: AtomicStatus,
    /// Listener for when the chain tip changes
    chain_tip_change: zebra_state::ChainTipChange,
}

impl StateService {
    /// Uses poll_ready to update the status of the `ReadStateService`.
    async fn fetch_status_from_validator(&self) -> StatusType {
        let mut read_state_service = self.read_state_service.clone();
        poll_fn(|cx| match read_state_service.poll_ready(cx) {
            std::task::Poll::Ready(Ok(())) => {
                self.status.store(StatusType::Ready.into());
                std::task::Poll::Ready(StatusType::Ready)
            }
            std::task::Poll::Ready(Err(e)) => {
                eprintln!("Service readiness error: {:?}", e);
                self.status.store(StatusType::CriticalError.into());
                std::task::Poll::Ready(StatusType::CriticalError)
            }
            std::task::Poll::Pending => {
                self.status.store(StatusType::Busy.into());
                std::task::Poll::Pending
            }
        })
        .await
    }
}

#[async_trait]
impl ZcashService for StateService {
    type Subscriber = StateServiceSubscriber;
    type Config = StateServiceConfig;

    /// Initializes a new StateService instance and starts sync process.
    async fn spawn(config: StateServiceConfig) -> Result<Self, StateServiceError> {
        info!("Launching Chain Fetch Service..");

        let rpc_client = JsonRpSeeConnector::new_from_config_parts(
            config.validator_cookie_auth,
            config.validator_rpc_address,
            config.validator_rpc_user.clone(),
            config.validator_rpc_password.clone(),
            config.validator_cookie_path.clone(),
        )
        .await?;

        let zebra_build_data = rpc_client.get_info().await?;

        // This const is optional, as the build script can only
        // generate it from hash-based dependencies.
        // in all other cases, this check will be skipped.
        if let Some(expected_zebrad_version) = crate::ZEBRA_VERSION {
            // this `+` indicates a git describe run
            // i.e. the first seven characters of the commit hash
            // have been appended. We match on those
            if zebra_build_data.build.contains('+') {
                if !zebra_build_data
                    .build
                    .contains(&expected_zebrad_version[0..7])
                {
                    return Err(StateServiceError::ZebradVersionMismatch {
                        expected_zebrad_version: expected_zebrad_version.to_string(),
                        connected_zebrad_version: zebra_build_data.build,
                    });
                }
            } else {
                // With no `+`, we expect a version number to be an exact match
                if expected_zebrad_version != zebra_build_data.build {
                    return Err(StateServiceError::ZebradVersionMismatch {
                        expected_zebrad_version: expected_zebrad_version.to_string(),
                        connected_zebrad_version: zebra_build_data.build,
                    });
                }
            }
        };
        let data = ServiceMetadata::new(
            get_build_info(),
            config.network.clone(),
            zebra_build_data.build,
            zebra_build_data.subversion,
        );
        info!("Using Zcash build: {}", data);

        info!("Launching Chain Syncer..");
        let (mut read_state_service, _latest_chain_tip, chain_tip_change, sync_task_handle) =
            init_read_state_with_syncer(
                config.validator_config.clone(),
                &config.network,
                config.validator_rpc_address,
            )
            .await??;

        // Wait for ReadStateService to catch up to primary database:
        loop {
            let server_height = rpc_client.get_blockchain_info().await?.blocks;

            let syncer_response = read_state_service
                .ready()
                .and_then(|service| service.call(ReadRequest::Tip))
                .await?;
            let (syncer_height, _) = expected_read_response!(syncer_response, Tip).ok_or(
                RpcError::new_from_legacycode(LegacyCode::Misc, "no blocks in chain"),
            )?;

            if server_height.0 == syncer_height.0 {
                break;
            } else {
                info!(" - ReadStateService syncing with Zebra. Syncer chain height: {}, Validator chain height: {}",
                            &syncer_height.0,
                            &server_height.0
                        );
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                continue;
            }
        }

        let block_cache = BlockCache::spawn(
            &rpc_client,
            Some(&read_state_service),
            config.clone().into(),
        )
        .await?;

        let mempool = Mempool::spawn(&rpc_client, None).await?;

        let state_service = Self {
            chain_tip_change,
            read_state_service,
            sync_task_handle: Some(Arc::new(sync_task_handle)),
            rpc_client: rpc_client.clone(),
            block_cache,
            mempool,
            data,
            config,
            status: AtomicStatus::new(StatusType::Spawning.into()),
        };

        state_service.status.store(StatusType::Ready.into());

        Ok(state_service)
    }

    /// returns a [`fetchservicesubscriber`].
    fn get_subscriber(&self) -> IndexerSubscriber<StateServiceSubscriber> {
        IndexerSubscriber::new(StateServiceSubscriber {
            read_state_service: self.read_state_service.clone(),
            rpc_client: self.rpc_client.clone(),
            block_cache: self.block_cache.subscriber(),
            mempool: self.mempool.subscriber(),
            data: self.data.clone(),
            config: self.config.clone(),
            chain_tip_change: self.chain_tip_change.clone(),
        })
    }

    /// Returns the StateService's Status.
    ///
    /// We first check for `status = StatusType::Closing` as this signifies a shutdown order
    /// from an external process.
    async fn status(&self) -> StatusType {
        let current_status = self.status.load().into();
        if current_status == StatusType::Closing {
            current_status
        } else {
            self.fetch_status_from_validator().await
        }
    }

    /// Shuts down the StateService.
    fn close(&mut self) {
        if self.sync_task_handle.is_some() {
            if let Some(handle) = self.sync_task_handle.take() {
                handle.abort();
            }
        }
    }
}

impl Drop for StateService {
    fn drop(&mut self) {
        self.close()
    }
}

/// A fetch service subscriber.
///
/// Subscribers should be
#[derive(Debug, Clone)]
pub struct StateServiceSubscriber {
    /// Remote wrappper functionality for zebra's [`ReadStateService`].
    pub read_state_service: ReadStateService,
    /// JsonRPC Client.
    pub rpc_client: JsonRpSeeConnector,
    /// Local compact block cache.
    pub block_cache: BlockCacheSubscriber,
    /// Internal mempool.
    pub mempool: MempoolSubscriber,
    /// Service metadata.
    pub data: ServiceMetadata,
    /// StateService config data.
    config: StateServiceConfig,
    /// Listener for when the chain tip changes
    chain_tip_change: zebra_state::ChainTipChange,
}

/// A subscriber to any chaintip updates
#[derive(Clone)]
pub struct ChainTipSubscriber(zebra_state::ChainTipChange);

impl ChainTipSubscriber {
    /// Waits until the tip hash has changed (relative to the last time this method
    /// was called), then returns the best tip's block hash.
    pub async fn next_tip_hash(
        &mut self,
    ) -> Result<zebra_chain::block::Hash, tokio::sync::watch::error::RecvError> {
        self.0
            .wait_for_tip_change()
            .await
            .map(|tip| tip.best_tip_hash())
    }
}

/// Private RPC methods, which are used as helper methods by the public ones
///
/// These would be simple to add to the public interface if
/// needed, there are currently no plans to do so.
impl StateServiceSubscriber {
    /// Gets a Subscriber to any updates to the latest chain tip
    pub fn chaintip_update_subscriber(&self) -> ChainTipSubscriber {
        ChainTipSubscriber(self.chain_tip_change.clone())
    }
    /// Returns the requested block header by hash or height, as a [`GetBlockHeader`] JSON string.
    /// If the block is not in Zebra's state,
    /// returns [error code `-8`.](https://github.com/zcash/zcash/issues/5758)
    /// if a height was passed or -5 if a hash was passed.
    ///
    /// zcashd reference: [`getblockheader`](https://zcash.github.io/rpc/getblockheader.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash_or_height`: (string, required, example="1") The hash or height
    ///   for the block to be returned.
    /// - `verbose`: (bool, optional, default=false, example=true) false for hex encoded data,
    ///   true for a json object
    ///
    /// # Notes
    ///
    /// The undocumented `chainwork` field is not returned.
    ///
    /// This rpc is used by get_block(verbose), there is currently no
    /// plan to offer this RPC publicly.
    async fn get_block_header(
        state: &ReadStateService,
        network: &Network,
        hash_or_height: HashOrHeight,
        verbose: Option<bool>,
    ) -> Result<GetBlockHeader, StateServiceError> {
        let mut state = state.clone();
        let verbose = verbose.unwrap_or(true);
        let network = network.clone();

        let zebra_state::ReadResponse::BlockHeader {
            header,
            hash,
            height,
            next_block_hash,
        } = state
            .ready()
            .and_then(|service| service.call(zebra_state::ReadRequest::BlockHeader(hash_or_height)))
            .await
            .map_err(|_| {
                StateServiceError::RpcError(RpcError {
                    // Compatibility with zcashd. Note that since this function
                    // is reused by getblock(), we return the errors expected
                    // by it (they differ whether a hash or a height was passed)
                    code: LegacyCode::InvalidParameter as i64,
                    message: "block height not in best chain".to_string(),
                    data: None,
                })
            })?
        else {
            return Err(StateServiceError::Custom(
                "Unexpected response to BlockHeader request".to_string(),
            ));
        };

        let response = if !verbose {
            GetBlockHeader::Raw(HexData(header.zcash_serialize_to_vec()?))
        } else {
            let zebra_state::ReadResponse::SaplingTree(sapling_tree) = state
                .ready()
                .and_then(|service| {
                    service.call(zebra_state::ReadRequest::SaplingTree(hash_or_height))
                })
                .await?
            else {
                return Err(StateServiceError::Custom(
                    "Unexpected response to SaplingTree request".to_string(),
                ));
            };
            // This could be `None` if there's a chain reorg between state queries.
            let sapling_tree = sapling_tree.ok_or_else(|| {
                StateServiceError::RpcError(zaino_fetch::jsonrpsee::connector::RpcError {
                    code: LegacyCode::InvalidParameter as i64,
                    message: "missing sapling tree for block".to_string(),
                    data: None,
                })
            })?;

            let zebra_state::ReadResponse::Depth(depth) = state
                .ready()
                .and_then(|service| service.call(zebra_state::ReadRequest::Depth(hash)))
                .await?
            else {
                return Err(StateServiceError::Custom(
                    "Unexpected response to Depth request".to_string(),
                ));
            };

            // From <https://zcash.github.io/rpc/getblock.html>
            // TODO: Deduplicate const definition, consider
            // refactoring this to avoid duplicate logic
            const NOT_IN_BEST_CHAIN_CONFIRMATIONS: i64 = -1;

            // Confirmations are one more than the depth.
            // Depth is limited by height, so it will never overflow an i64.
            let confirmations = depth
                .map(|depth| i64::from(depth) + 1)
                .unwrap_or(NOT_IN_BEST_CHAIN_CONFIRMATIONS);

            let mut nonce = *header.nonce;
            nonce.reverse();

            let sapling_activation = NetworkUpgrade::Sapling.activation_height(&network);
            let sapling_tree_size = sapling_tree.count();
            let final_sapling_root: [u8; 32] =
                if sapling_activation.is_some() && height >= sapling_activation.unwrap() {
                    let mut root: [u8; 32] = sapling_tree.root().into();
                    root.reverse();
                    root
                } else {
                    [0; 32]
                };

            let difficulty = header.difficulty_threshold.relative_to_network(&network);
            let block_commitments =
                header_to_block_commitments(&header, &network, height, final_sapling_root)?;

            let block_header = GetBlockHeaderObject {
                hash: GetBlockHash(hash),
                confirmations,
                height,
                version: header.version,
                merkle_root: header.merkle_root,
                final_sapling_root,
                sapling_tree_size,
                time: header.time.timestamp(),
                nonce,
                solution: header.solution,
                bits: header.difficulty_threshold,
                difficulty,
                previous_block_hash: GetBlockHash(header.previous_block_hash),
                next_block_hash: next_block_hash.map(GetBlockHash),
                block_commitments,
            };

            GetBlockHeader::Object(Box::new(block_header))
        };

        Ok(response)
    }

    /// Return a list of consecutive compact blocks.
    #[allow(dead_code)]
    async fn get_block_range_inner(
        &self,
        request: BlockRange,
        trim_non_nullifier: bool,
    ) -> Result<CompactBlockStream, StateServiceError> {
        let mut start: u32 = match request.start {
            Some(block_id) => match block_id.height.try_into() {
                Ok(height) => height,
                Err(_) => {
                    return Err(StateServiceError::TonicStatusError(
                        tonic::Status::invalid_argument(
                            "Error: Start height out of range. Failed to convert to u32.",
                        ),
                    ));
                }
            },
            None => {
                return Err(StateServiceError::TonicStatusError(
                    tonic::Status::invalid_argument("Error: No start height given."),
                ));
            }
        };
        let mut end: u32 = match request.end {
            Some(block_id) => match block_id.height.try_into() {
                Ok(height) => height,
                Err(_) => {
                    return Err(StateServiceError::TonicStatusError(
                        tonic::Status::invalid_argument(
                            "Error: End height out of range. Failed to convert to u32.",
                        ),
                    ));
                }
            },
            None => {
                return Err(StateServiceError::TonicStatusError(
                    tonic::Status::invalid_argument("Error: No start height given."),
                ));
            }
        };
        let lowest_to_highest = if start > end {
            (start, end) = (end, start);
            false
        } else {
            true
        };
        let chain_height = self.block_cache.get_chain_height().await?.0;
        let fetch_service_clone = self.clone();
        let service_timeout = self.config.service_timeout;
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    let mut blocks = NonEmpty::new(
                        match fetch_service_clone
                            .block_cache
                            .get_compact_block(end.to_string())
                            .await
                        {
                            Ok(mut block) => {
                                if trim_non_nullifier {
                                    block = compact_block_to_nullifiers(block);
                                }
                                Ok(block)
                            }
                            Err(e) => {
                                if end >= chain_height {
                                    Err(tonic::Status::out_of_range(format!(
                                        "Error: Height out of range [{}]. Height \
                                            requested is greater than the best \
                                            chain tip [{}].",
                                        end, chain_height,
                                    )))
                                } else {
                                    Err(tonic::Status::unknown(e.to_string()))
                                }
                            }
                        },
                    );
                    for i in start..end {
                        let Ok(child_block) = blocks.last() else {
                            break;
                        };
                        let Ok(hash_or_height) =
                            <[u8; 32]>::try_from(child_block.prev_hash.as_slice())
                                .map(zebra_chain::block::Hash)
                                .map(HashOrHeight::from)
                        else {
                            break;
                        };
                        blocks.push(
                            match fetch_service_clone
                                .block_cache
                                .get_compact_block(hash_or_height.to_string())
                                .await
                            {
                                Ok(mut block) => {
                                    if trim_non_nullifier {
                                        block = compact_block_to_nullifiers(block);
                                    }
                                    Ok(block)
                                }
                                Err(e) => {
                                    let height = end - (i - start);
                                    if height >= chain_height {
                                        Err(tonic::Status::out_of_range(format!(
                                            "Error: Height out of range [{}]. Height requested \
                                            is greater than the best chain tip [{}].",
                                            height, chain_height,
                                        )))
                                    } else {
                                        Err(tonic::Status::unknown(e.to_string()))
                                    }
                                }
                            },
                        );
                    }
                    if lowest_to_highest {
                        blocks = NonEmpty::from_vec(blocks.into_iter().rev().collect::<Vec<_>>())
                            .expect("known to be non-empty")
                    }
                    for block in blocks {
                        if let Err(e) = channel_tx.send(block).await {
                            warn!("GetBlockRange channel closed unexpectedly: {e}");
                            break;
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
                            "Error: get_block_range gRPC request timed out.",
                        )))
                        .await
                        .ok();
                }
            }
        });
        Ok(CompactBlockStream::new(channel_rx))
    }

    async fn error_get_block(
        &self,
        e: BlockCacheError,
        height: u32,
    ) -> Result<CompactBlock, StateServiceError> {
        let chain_height = self.block_cache.get_chain_height().await?.0;
        Err(if height >= chain_height {
            StateServiceError::TonicStatusError(tonic::Status::out_of_range(format!(
                "Error: Height out of range [{}]. Height requested \
                                is greater than the best chain tip [{}].",
                height, chain_height,
            )))
        } else {
            // TODO: Hide server error from clients before release.
            // Currently useful for dev purposes.
            StateServiceError::TonicStatusError(tonic::Status::unknown(format!(
                "Error: Failed to retrieve block from node. Server Error: {}",
                e,
            )))
        })
    }

    pub(crate) async fn get_block_inner(
        state: &ReadStateService,
        network: &Network,
        hash_or_height: HashOrHeight,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, StateServiceError> {
        let mut state_1 = state.clone();

        let verbosity = verbosity.unwrap_or(1);
        match verbosity {
            0 => {
                let request = ReadRequest::Block(hash_or_height);
                let response = state_1
                    .ready()
                    .and_then(|service| service.call(request))
                    .await?;
                let block = expected_read_response!(response, Block);
                block.map(SerializedBlock::from).map(GetBlock::Raw).ok_or(
                    StateServiceError::RpcError(RpcError::new_from_legacycode(
                        LegacyCode::InvalidParameter,
                        "block not found",
                    )),
                )
            }
            1 | 2 => {
                let state_2 = state.clone();
                let state_3 = state.clone();

                let txids_or_fullblock_request = match verbosity {
                    1 => ReadRequest::TransactionIdsForBlock(hash_or_height),
                    2 => ReadRequest::Block(hash_or_height),
                    _ => unreachable!("verbosity is known to be 1 or 2"),
                };
                let txids_future = {
                    let req = txids_or_fullblock_request;
                    async move { state_1.ready().and_then(|service| service.call(req)).await }
                };
                let orchard_future = {
                    let req = ReadRequest::OrchardTree(hash_or_height);
                    async move {
                        state_2
                            .clone()
                            .ready()
                            .and_then(|service| service.call(req))
                            .await
                    }
                };
                let (txids_or_fullblock, orchard_tree_response, header) = futures::join!(
                    txids_future,
                    orchard_future,
                    StateServiceSubscriber::get_block_header(
                        &state_3,
                        network,
                        hash_or_height,
                        Some(true)
                    )
                );

                let header_obj = match header? {
                    GetBlockHeader::Raw(_hex_data) => unreachable!(
                        "`true` was passed to get_block_header, an object should be returned"
                    ),
                    GetBlockHeader::Object(get_block_header_object) => get_block_header_object,
                };
                let GetBlockHeaderObject {
                    hash,
                    confirmations,
                    height,
                    version,
                    merkle_root,
                    final_sapling_root,
                    sapling_tree_size,
                    time,
                    nonce,
                    solution,
                    bits,
                    difficulty,
                    previous_block_hash,
                    next_block_hash,
                    block_commitments,
                } = *header_obj;

                let transactions_response: Vec<GetBlockTransaction> = match txids_or_fullblock {
                    Ok(ReadResponse::TransactionIdsForBlock(Some(txids))) => Ok(txids
                        .iter()
                        .copied()
                        .map(GetBlockTransaction::Hash)
                        .collect()),
                    Ok(ReadResponse::Block(Some(block))) => Ok(block
                        .transactions
                        .iter()
                        .map(|transaction| {
                            GetBlockTransaction::Object(Box::new(
                                TransactionObject::from_transaction(
                                    transaction.clone(),
                                    Some(height),
                                    Some(confirmations as u32),
                                    network,
                                    DateTime::<Utc>::from_timestamp(time, 0),
                                ),
                            ))
                        })
                        .collect()),
                    Ok(ReadResponse::TransactionIdsForBlock(None))
                    | Ok(ReadResponse::Block(None)) => {
                        Err(StateServiceError::RpcError(RpcError::new_from_legacycode(
                            LegacyCode::InvalidParameter,
                            "block not found",
                        )))
                    }
                    Ok(unexpected) => {
                        unreachable!("Unexpected response from state service: {unexpected:?}")
                    }
                    Err(e) => Err(e.into()),
                }?;

                let orchard_tree_response = orchard_tree_response?;
                let orchard_tree = expected_read_response!(orchard_tree_response, OrchardTree)
                    .ok_or(StateServiceError::RpcError(RpcError::new_from_legacycode(
                        LegacyCode::Misc,
                        "missing orchard tree",
                    )))?;

                let final_orchard_root = match NetworkUpgrade::Nu5.activation_height(network) {
                    Some(activation_height) if height >= activation_height => {
                        Some(orchard_tree.root().into())
                    }
                    _otherwise => None,
                };

                let trees = GetBlockTrees::new(sapling_tree_size, orchard_tree.count());

                Ok(GetBlock::Object {
                    hash,
                    confirmations,
                    height: Some(height),
                    version: Some(version),
                    merkle_root: Some(merkle_root),
                    time: Some(time),
                    nonce: Some(nonce),
                    solution: Some(solution),
                    bits: Some(bits),
                    difficulty: Some(difficulty),
                    tx: transactions_response,
                    trees,
                    size: None,
                    final_sapling_root: Some(final_sapling_root),
                    final_orchard_root,
                    previous_block_hash: Some(previous_block_hash),
                    next_block_hash,
                    block_commitments: Some(block_commitments),
                })
            }
            more_than_two => Err(StateServiceError::RpcError(RpcError::new_from_legacycode(
                LegacyCode::InvalidParameter,
                format!("invalid verbosity of {more_than_two}"),
            ))),
        }
    }
}

#[async_trait]
impl ZcashIndexer for StateServiceSubscriber {
    type Error = StateServiceError;

    async fn get_info(&self) -> Result<GetInfo, Self::Error> {
        // A number of these fields are difficult to access from the state service
        // TODO: Fix this
        self.rpc_client
            .get_info()
            .await
            .map(GetInfo::from)
            .map_err(Self::Error::from)
    }

    async fn get_blockchain_info(&self) -> Result<GetBlockChainInfo, Self::Error> {
        let mut state = self.read_state_service.clone();

        let response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::TipPoolValues))
            .await?;
        let (height, hash, balance) = match response {
            ReadResponse::TipPoolValues {
                tip_height,
                tip_hash,
                value_balance,
            } => (tip_height, tip_hash, value_balance),
            unexpected => {
                unreachable!("Unexpected response from state service: {unexpected:?}")
            }
        };

        let usage_response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::UsageInfo))
            .await?;
        let size_on_disk = expected_read_response!(usage_response, UsageInfo);

        let request = zebra_state::ReadRequest::BlockHeader(hash.into());
        let response = state
            .ready()
            .and_then(|service| service.call(request))
            .await?;
        let header = match response {
            ReadResponse::BlockHeader { header, .. } => header,
            unexpected => {
                unreachable!("Unexpected response from state service: {unexpected:?}")
            }
        };

        let now = Utc::now();
        let zebra_estimated_height =
            NetworkChainTipHeightEstimator::new(header.time, height, &self.config.network)
                .estimate_height_at(now);
        let estimated_height = if header.time > now || zebra_estimated_height < height {
            height
        } else {
            zebra_estimated_height
        };

        let upgrades = IndexMap::from_iter(
            self.config
                .network
                .full_activation_list()
                .into_iter()
                .filter_map(|(activation_height, network_upgrade)| {
                    // Zebra defines network upgrades based on incompatible consensus rule changes,
                    // but zcashd defines them based on ZIPs.
                    //
                    // All the network upgrades with a consensus branch ID
                    // are the same in Zebra and zcashd.
                    network_upgrade.branch_id().map(|branch_id| {
                        // zcashd's RPC seems to ignore Disabled network upgrades,
                        // so Zebra does too.
                        let status = if height >= activation_height {
                            NetworkUpgradeStatus::Active
                        } else {
                            NetworkUpgradeStatus::Pending
                        };

                        (
                            ConsensusBranchIdHex::new(branch_id.into()),
                            NetworkUpgradeInfo::from_parts(
                                network_upgrade,
                                activation_height,
                                status,
                            ),
                        )
                    })
                }),
        );

        let next_block_height =
            (height + 1).expect("valid chain tips are a lot less than Height::MAX");
        let consensus = TipConsensusBranch::from_parts(
            ConsensusBranchIdHex::new(
                NetworkUpgrade::current(&self.config.network, height)
                    .branch_id()
                    .unwrap_or(ConsensusBranchId::RPC_MISSING_ID)
                    .into(),
            )
            .inner(),
            ConsensusBranchIdHex::new(
                NetworkUpgrade::current(&self.config.network, next_block_height)
                    .branch_id()
                    .unwrap_or(ConsensusBranchId::RPC_MISSING_ID)
                    .into(),
            )
            .inner(),
        );

        // TODO: Remove unwrap()
        let difficulty = chain_tip_difficulty(
            self.config.network.clone(),
            self.read_state_service.clone(),
            false,
        )
        .await
        .unwrap();

        let verification_progress = f64::from(height.0) / f64::from(zebra_estimated_height.0);

        Ok(GetBlockChainInfo::new(
            self.config.network.bip70_network_name(),
            height,
            hash,
            estimated_height,
            zebra_rpc::methods::types::get_blockchain_info::Balance::chain_supply(balance),
            zebra_rpc::methods::types::get_blockchain_info::Balance::value_pools(balance),
            upgrades,
            consensus,
            height,
            difficulty,
            verification_progress,
            // TODO: store work in the finalized state for each height
            // see https://github.com/ZcashFoundation/zebra/issues/7109
            0,
            false,
            size_on_disk,
            // TODO (copied from zebra): Investigate whether this needs to
            // be implemented (it's sprout-only in zcashd)
            0,
        ))
    }

    async fn z_get_address_balance(
        &self,
        address_strings: AddressStrings,
    ) -> Result<AddressBalance, Self::Error> {
        let mut state = self.read_state_service.clone();

        let strings_set = address_strings
            .valid_addresses()
            .map_err(|e| RpcError::new_from_errorobject(e, "invalid taddrs provided"))?;
        let response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::AddressBalance(strings_set)))
            .await?;
        let balance = expected_read_response!(response, AddressBalance);

        Ok(AddressBalance {
            balance: u64::from(balance),
        })
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<SentTransactionHash, Self::Error> {
        // Offload to the json rpc connector, as ReadStateService
        // doesn't yet interface with the mempool
        self.rpc_client
            .send_raw_transaction(raw_transaction_hex)
            .await
            .map(SentTransactionHash::from)
            .map_err(StateServiceError::JsonRpcConnectorError)
    }

    async fn z_get_block(
        &self,
        hash_or_height_string: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        let hash_or_height = HashOrHeight::from_str(&hash_or_height_string);

        StateServiceSubscriber::get_block_inner(
            &self.read_state_service.clone(),
            &self.data.network(),
            hash_or_height?,
            verbosity,
        )
        .await
    }

    async fn get_raw_mempool(&self) -> Result<Vec<String>, Self::Error> {
        Ok(self
            .mempool
            .get_mempool()
            .await
            .into_iter()
            .map(|(key, _)| key.0)
            .collect())
    }

    async fn z_get_treestate(&self, hash_or_height: String) -> Result<GetTreestate, Self::Error> {
        let mut state = self.read_state_service.clone();

        let hash_or_height = HashOrHeight::from_str(&hash_or_height)?;
        let block_header_response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::BlockHeader(hash_or_height)))
            .await?;
        let (header, hash, height) = match block_header_response {
            ReadResponse::BlockHeader {
                header,
                hash,
                height,
                ..
            } => (header, hash, height),
            unexpected => {
                unreachable!("Unexpected response from state service: {unexpected:?}")
            }
        };

        let sapling = match NetworkUpgrade::Sapling.activation_height(&self.config.network) {
            Some(activation_height) if height >= activation_height => Some(
                state
                    .ready()
                    .and_then(|service| service.call(ReadRequest::SaplingTree(hash_or_height)))
                    .await?,
            ),
            _ => None,
        }
        .and_then(|sap_response| {
            expected_read_response!(sap_response, SaplingTree).map(|tree| tree.to_rpc_bytes())
        });

        let orchard = match NetworkUpgrade::Nu5.activation_height(&self.config.network) {
            Some(activation_height) if height >= activation_height => Some(
                state
                    .ready()
                    .and_then(|service| service.call(ReadRequest::OrchardTree(hash_or_height)))
                    .await?,
            ),
            _ => None,
        }
        .and_then(|orch_response| {
            expected_read_response!(orch_response, OrchardTree).map(|tree| tree.to_rpc_bytes())
        });

        Ok(GetTreestate::from_parts(
            hash,
            height,
            // If the timestamp is pre-unix epoch, something has gone terribly wrong
            u32::try_from(header.time.timestamp()).unwrap(),
            sapling,
            orchard,
        ))
    }

    /// Returns the current block count in the best valid block chain.
    ///
    /// zcashd reference: [`getblockcount`](https://zcash.github.io/rpc/getblockcount.html)
    /// method: post
    /// tags: blockchain
    async fn get_block_count(&self) -> Result<Height, Self::Error> {
        Ok(self.block_cache.get_chain_height().await?)
    }

    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<GetSubtrees, Self::Error> {
        let mut state = self.read_state_service.clone();

        match pool.as_str() {
            "sapling" => {
                let request = zebra_state::ReadRequest::SaplingSubtrees { start_index, limit };
                let response = state
                    .ready()
                    .and_then(|service| service.call(request))
                    .await?;
                let sapling_subtrees = expected_read_response!(response, SaplingSubtrees);
                let subtrees = sapling_subtrees
                    .values()
                    .map(|subtree| SubtreeRpcData {
                        root: subtree.root.encode_hex(),
                        end_height: subtree.end_height,
                    })
                    .collect();

                Ok(GetSubtrees {
                    pool,
                    start_index,
                    subtrees,
                })
            }
            "orchard" => {
                let request = zebra_state::ReadRequest::OrchardSubtrees { start_index, limit };
                let response = state
                    .ready()
                    .and_then(|service| service.call(request))
                    .await?;
                let orchard_subtrees = expected_read_response!(response, OrchardSubtrees);
                let subtrees = orchard_subtrees
                    .values()
                    .map(|subtree| SubtreeRpcData {
                        root: subtree.root.encode_hex(),
                        end_height: subtree.end_height,
                    })
                    .collect();

                Ok(GetSubtrees {
                    pool,
                    start_index,
                    subtrees,
                })
            }
            otherwise => Err(StateServiceError::RpcError(RpcError::new_from_legacycode(
                LegacyCode::Misc,
                format!("invalid pool name \"{otherwise}\", must be \"sapling\" or \"orchard\""),
            ))),
        }
    }

    async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetRawTransaction, Self::Error> {
        let mut state = self.read_state_service.clone();

        let txid = zebra_chain::transaction::Hash::from_hex(txid_hex).map_err(|e| {
            RpcError::new_from_legacycode(LegacyCode::InvalidAddressOrKey, e.to_string())
        })?;

        let not_found_error = || {
            StateServiceError::RpcError(RpcError::new_from_legacycode(
                LegacyCode::InvalidAddressOrKey,
                "No such mempool or main chain transaction",
            ))
        };

        // First check if transaction is in mempool as this is quick.
        match self
            .mempool
            .contains_txid(&MempoolKey(txid.to_string()))
            .await
        {
            // Fetch trasaction from mempool.
            true => {
                match self
                    .mempool
                    .get_transaction(&MempoolKey(txid.to_string()))
                    .await
                {
                    Some(tx) => {
                        if let GetRawTransaction::Object(transaction_object) = &tx.0 {
                            Ok(GetRawTransaction::Object(transaction_object.clone()))
                        } else {
                            unreachable!("unmatched response to a `Transaction` read request")
                        }
                    }
                    None => Err(not_found_error()),
                }
            }
            // Fetch transaction from state.
            false => {
                //
                match state
                    .ready()
                    .and_then(|service| service.call(zebra_state::ReadRequest::Transaction(txid)))
                    .await
                    .map_err(|_| not_found_error())?
                {
                    zebra_state::ReadResponse::Transaction(Some(tx)) => Ok(match verbose {
                        Some(_verbosity) => GetRawTransaction::Object(Box::new(
                            TransactionObject::from_transaction(
                                tx.tx.clone(),
                                Some(tx.height),
                                Some(tx.confirmations),
                                &self.config.network,
                                Some(tx.block_time),
                            ),
                        )),
                        None => GetRawTransaction::Raw(tx.tx.into()),
                    }),
                    zebra_state::ReadResponse::Transaction(None) => Err(not_found_error()),

                    _ => unreachable!("unmatched response to a `Transaction` read request"),
                }
            }
        }
    }

    async fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, Self::Error> {
        let mut state = self.read_state_service.clone();

        let (addresses, start, end) = request.into_parts();
        let response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::Tip))
            .await?;
        let (chain_height, _chain_hash) = expected_read_response!(response, Tip).ok_or(
            RpcError::new_from_legacycode(LegacyCode::Misc, "no blocks in chain"),
        )?;

        let mut error_string = None;
        if start == 0 || end == 0 {
            error_string = Some(format!(
                "start {start:?} and end {end:?} must both be greater than zero"
            ));
        }
        if start > end {
            error_string = Some(format!(
                "start {start:?} must be less than or equal to end {end:?}"
            ));
        }
        if Height(start) > chain_height || Height(end) > chain_height {
            error_string = Some(format!(
                "start {start:?} and end {end:?} must both be less than or \
            equal to the chain tip {chain_height:?}"
            ));
        }
        if let Some(e) = error_string {
            return Err(StateServiceError::RpcError(RpcError::new_from_legacycode(
                LegacyCode::InvalidParameter,
                e,
            )));
        }

        let request = ReadRequest::TransactionIdsByAddresses {
            addresses: AddressStrings::new_valid(addresses)
                .and_then(|addrs| addrs.valid_addresses())
                .map_err(|e| RpcError::new_from_errorobject(e, "invalid adddress"))?,

            height_range: Height(start)..=Height(end),
        };
        let response = state
            .ready()
            .and_then(|service| service.call(request))
            .await?;
        let hashes = expected_read_response!(response, AddressesTransactionIds);

        let mut last_tx_location = TransactionLocation::from_usize(Height(0), 0);

        Ok(hashes
            .iter()
            .map(|(tx_loc, tx_id)| {
                // Check that the returned transactions are in chain order.
                assert!(
                    *tx_loc > last_tx_location,
                    "Transactions were not in chain order:\n\
                                 {tx_loc:?} {tx_id:?} was after:\n\
                                 {last_tx_location:?}",
                );

                last_tx_location = *tx_loc;

                tx_id.to_string()
            })
            .collect())
    }

    async fn z_get_address_utxos(
        &self,
        address_strings: AddressStrings,
    ) -> Result<Vec<GetAddressUtxos>, Self::Error> {
        let mut state = self.read_state_service.clone();

        let valid_addresses = address_strings
            .valid_addresses()
            .map_err(|e| RpcError::new_from_errorobject(e, "invalid address"))?;
        let request = ReadRequest::UtxosByAddresses(valid_addresses);
        let response = state
            .ready()
            .and_then(|service| service.call(request))
            .await?;
        let utxos = expected_read_response!(response, AddressUtxos);
        let mut last_output_location = OutputLocation::from_usize(Height(0), 0, 0);

        Ok(utxos
            .utxos()
            .map(|utxo| {
                assert!(utxo.2 > &last_output_location);
                last_output_location = *utxo.2;
                // What an odd argument order for from_parts
                // at least they are all different types, so they can't be
                // supplied in the wrong order
                GetAddressUtxos::from_parts(
                    utxo.0,
                    *utxo.1,
                    utxo.2.output_index(),
                    utxo.3.lock_script.clone(),
                    u64::from(utxo.3.value()),
                    utxo.2.height(),
                )
            })
            .collect())
    }

    // Helper function, to get the chain height in rpc implementations
    async fn chain_height(&self) -> Result<Height, Self::Error> {
        let mut state = self.read_state_service.clone();
        let response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::Tip))
            .await?;
        let (chain_height, _chain_hash) = expected_read_response!(response, Tip).ok_or(
            RpcError::new_from_legacycode(LegacyCode::Misc, "no blocks in chain"),
        )?;
        Ok(chain_height)
    }
}

#[async_trait]
impl LightWalletIndexer for StateServiceSubscriber {
    /// Return the height of the tip of the best chain
    async fn get_latest_block(&self) -> Result<BlockId, Self::Error> {
        let mut state = self.read_state_service.clone();
        let response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::Tip))
            .await?;
        let (chain_height, chain_hash) = expected_read_response!(response, Tip).ok_or(
            RpcError::new_from_legacycode(LegacyCode::Misc, "no blocks in chain"),
        )?;
        Ok(BlockId {
            height: chain_height.as_usize() as u64,
            hash: chain_hash.0.to_vec(),
        })
    }

    /// Return the compact block corresponding to the given block identifier
    async fn get_block(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        let height = request.height;
        let hash_or_height = blockid_to_hashorheight(request).ok_or(
            StateServiceError::TonicStatusError(tonic::Status::invalid_argument(
                "Error: Invalid hash and/or height out of range. Failed to convert to u32.",
            )),
        )?;
        match self
            .block_cache
            .get_compact_block(hash_or_height.to_string())
            .await
        {
            Ok(block) => Ok(block),
            Err(e) => self.error_get_block(e, height as u32).await,
        }
    }

    /// Same as GetBlock except actions contain only nullifiers,
    /// and saling outputs are not returned (Sapling spends still are)
    async fn get_block_nullifiers(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        let height: u32 = match request.height.try_into() {
            Ok(height) => height,
            Err(_) => {
                return Err(StateServiceError::TonicStatusError(
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
            Err(e) => self.error_get_block(e, height).await,
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
        <Self as ZcashIndexer>::get_raw_transaction(self, hex, Some(1))
            .await
            .and_then(|grt| match grt {
                GetRawTransaction::Raw(_serialized_transaction) => Err(StateServiceError::Custom(
                    "unreachable, verbose transaction expected".to_string(),
                )),
                GetRawTransaction::Object(transaction_object) => Ok(RawTransaction {
                    data: transaction_object.hex.as_ref().to_vec(),
                    height: transaction_object.height.unwrap_or(0) as u64,
                }),
            })
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
        let txids = self.get_taddress_txids_helper(request).await?;
        let chain_height = self.chain_height().await?;
        let (transmitter, receiver) = mpsc::channel(self.config.service_channel_size as usize);
        let service_timeout = self.config.service_timeout;
        let service_clone = self.clone();
        tokio::spawn(async move {
            let timeout = timeout(
                std::time::Duration::from_secs((service_timeout * 4) as u64),
                async {
                    for txid in txids {
                        let transaction = service_clone.get_raw_transaction(txid, Some(1)).await;
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
                        .send(Err(tonic::Status::deadline_exceeded(
                            "Error: get_mempool_stream gRPC request timed out",
                        )))
                        .await
                        .ok();
                }
            }
        });
        Ok(RawTransactionStream::new(receiver))
    }

    /// Returns the total balance for a list of taddrs
    async fn get_taddress_balance(
        &self,
        request: AddressList,
    ) -> Result<zaino_proto::proto::service::Balance, Self::Error> {
        let taddrs = AddressStrings::new_valid(request.addresses).map_err(|err_obj| {
            Self::Error::RpcError(RpcError::new_from_errorobject(
                err_obj,
                "Error in Validator",
            ))
        })?;
        let balance = self.z_get_address_balance(taddrs).await?;
        let checked_balance: i64 = match i64::try_from(balance.balance) {
            Ok(balance) => balance,
            Err(_) => {
                return Err(Self::Error::TonicStatusError(tonic::Status::unknown(
                    "Error: Error converting balance from u64 to i64.",
                )));
            }
        };
        Ok(zaino_proto::proto::service::Balance {
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
                                        StateServiceError::RpcError(RpcError::new_from_errorobject(
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
                        // TODO: Hide server error from clients before release. Currently useful for dev purposes.
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
            // TODO: Hide server error from clients before release. Currently useful for dev purposes.
            Err(e) => Err(StateServiceError::TonicStatusError(tonic::Status::unknown(
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
                time::Duration::from_secs((service_timeout * 4) as u64),
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
                                    Some(vec![txid_bytes]),
                                    None,
                                ) {
                                    Ok(transaction) => {
                                        // ParseFromSlice returns any data left after
                                        // the conversion to a FullTransaction, If the
                                        // conversion has succeeded this should be empty.
                                        if transaction.0.is_empty() {
                                            if channel_tx
                                                .send(transaction.1.to_compact(0).map_err(|e| {
                                                    tonic::Status::unknown(e.to_string())
                                                }))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        } else {
                                            // TODO: Hide server error from clients before
                                            // release. Currently useful for dev purposes.
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
                                        // TODO: Hide server error from clients
                                        // before release. Currently useful for dev purposes.
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
                                        "Error: Received raw transaction type, \
                                        this should not be possible.",
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
                time::Duration::from_secs((service_timeout * 6) as u64),
                async {
                    let (mut mempool_stream, _mempool_handle) = match mempool
                        .get_mempool_stream()
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
                            Ok((_mempool_key, mempool_value)) => match mempool_value.0 {
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
                                            "Error: Received raw transaction type, \
                                                this should not be possible.",
                                        )))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            },
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
        let hash_or_height = blockid_to_hashorheight(request).ok_or(
            crate::error::StateServiceError::TonicStatusError(tonic::Status::invalid_argument(
                "Invalid hash or height",
            )),
        )?;
        let (hash, height, time, sapling, orchard) =
            <StateServiceSubscriber as ZcashIndexer>::z_get_treestate(
                self,
                hash_or_height.to_string(),
            )
            .await?
            .into_parts();
        Ok(TreeState {
            network: self.config.network.bip70_network_name(),
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
            self.config.service_timeout,
            self.config.service_channel_size,
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
        self.get_address_utxos_stream(request)
            .await?
            .try_collect::<Vec<_>>()
            .await
            .map(|address_utxos| GetAddressUtxosReplyList { address_utxos })
            .map_err(Self::Error::from)
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
        let mut state = self.read_state_service.clone();
        let mut address_set = HashSet::new();
        for address in request.addresses {
            address_set.insert(zebra_chain::transparent::Address::from_str(
                address.as_ref(),
            )?);
        }

        let address_utxos_response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::UtxosByAddresses(address_set)))
            .await?;
        let utxos = expected_read_response!(address_utxos_response, AddressUtxos);
        let (channel_tx, channel_rx) = mpsc::channel(self.config.service_channel_size as usize);
        tokio::spawn(async move {
            for utxo in utxos
                .utxos()
                .filter_map(|(address, hash, location, output)| {
                    if location.height().0 as u64 >= request.start_height {
                        Some(GetAddressUtxosReply {
                            address: address.to_string(),
                            txid: hash.0.to_vec(),
                            index: location.output_index().index() as i32,
                            script: output.lock_script.as_raw_bytes().to_vec(),
                            value_zat: output.value.zatoshis(),
                            height: location.height().0 as u64,
                        })
                    } else {
                        None
                    }
                })
                .take(match usize::try_from(request.max_entries) {
                    Ok(0) | Err(_) => usize::MAX,
                    Ok(non_zero) => non_zero,
                })
            {
                if channel_tx.send(Ok(utxo)).await.is_err() {
                    return;
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

#[allow(clippy::result_large_err)]
fn header_to_block_commitments(
    header: &Header,
    network: &Network,
    height: Height,
    final_sapling_root: [u8; 32],
) -> Result<[u8; 32], StateServiceError> {
    let hash = match header.commitment(network, height).map_err(|e| {
        StateServiceError::SerializationError(
            zebra_chain::serialization::SerializationError::Parse(
                // For some reason this error type takes a
                // &'static str, and the the only way to create one
                // dynamically is to leak a String. This shouldn't
                // be a concern, as this error case should
                // never occur when communing with a zebrad, which
                // validates this field before serializing it
                e.to_string().leak(),
            ),
        )
    })? {
        zebra_chain::block::Commitment::PreSaplingReserved(bytes) => bytes,
        zebra_chain::block::Commitment::FinalSaplingRoot(_root) => final_sapling_root,
        zebra_chain::block::Commitment::ChainHistoryActivationReserved => [0; 32],
        zebra_chain::block::Commitment::ChainHistoryRoot(root) => root.bytes_in_display_order(),
        zebra_chain::block::Commitment::ChainHistoryBlockTxAuthCommitment(hash) => {
            hash.bytes_in_display_order()
        }
    };
    Ok(hash)
}
