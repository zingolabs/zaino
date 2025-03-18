//! Remote Zcash chain fetch and tx submission service backed by Zebras [`RemoteStateService`].

use crate::{
    config::FetchServiceConfig,
    error::RemoteStateServiceError,
    expected_read_response,
    indexer::{IndexerSubscriber, LightWalletIndexer, ZcashIndexer, ZcashService},
    local_cache::{BlockCache, BlockCacheSubscriber},
    mempool::{Mempool, MempoolSubscriber},
    state::header_to_block_commitments,
    status::StatusType,
    stream::{
        AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
        SubtreeRootReplyStream, UtxoReplyStream,
    },
    utils::{get_build_info, ServiceMetadata},
};

use zaino_fetch::{
    chain::{transaction::FullTransaction, utils::ParseFromSlice},
    jsonrpc::{
        connector::{test_node_and_return_url, JsonRpcConnector, RpcError},
        response::TxidsResponse,
    },
};
use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, BlockId, BlockRange, Duration, Exclude, GetAddressUtxosArg,
        GetAddressUtxosReplyList, GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction,
        SendResponse, TransparentAddressBlockFilter, TreeState, TxFilter,
    },
};

use zebra_chain::{
    block::{Height, SerializedBlock},
    chain_tip::NetworkChainTipHeightEstimator,
    parameters::{ConsensusBranchId, NetworkUpgrade},
    serialization::ZcashSerialize,
    subtree::NoteCommitmentSubtreeIndex,
};
use zebra_rpc::{
    methods::{
        hex_data::HexData,
        trees::{GetSubtrees, GetTreestate, SubtreeRpcData},
        AddressBalance, AddressStrings, ConsensusBranchIdHex, GetAddressTxIdsRequest,
        GetAddressUtxos, GetBlock, GetBlockChainInfo, GetBlockHash, GetBlockHeader,
        GetBlockHeaderObject, GetBlockTransaction, GetBlockTrees, GetInfo, GetRawTransaction,
        NetworkUpgradeInfo, NetworkUpgradeStatus, SentTransactionHash, TipConsensusBranch,
        TransactionObject,
    },
    server::error::LegacyCode,
};
use zebra_state::{
    HashOrHeight, MinedTx, OutputLocation, ReadRequest, ReadResponse, TransactionLocation,
};

use chrono::Utc;
use futures::{FutureExt, TryFutureExt};
use hex::{FromHex, ToHex};
use indexmap::IndexMap;
use std::str::FromStr;
use tokio::{
    sync::mpsc,
    time::{self, timeout},
};
use tonic::async_trait;
use tower::{Service, ServiceExt};
use tracing::{info, warn};

/// Remote Chain fetch service backed by Zebrad's [`RemoteStateService`].
///
/// This service is a central service, [`RemoteStateServiceSubscriber`] should be created to fetch data.
/// This is done to enable large numbers of concurrent subscribers without significant slowdowns.
///
/// NOTE: We currently do not implement clone for chain fetch services as this service is responsible for maintaining and closing its child processes.
///       ServiceSubscribers are used to create separate chain fetch processes while allowing central state processes to be managed in a single place.
///       If we want the ability to clone Service all JoinHandle's should be converted to Arc<JoinHandle>.
#[derive(Debug)]
pub struct RemoteStateService {
    /// Remote wrappper functionality for zebra's [`ReadStateService`].
    remote_state_service: zebra_state::RemoteStateService,
    /// JsonRPC Client.
    rpc_client: JsonRpcConnector,
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
impl ZcashService for RemoteStateService {
    type Error = RemoteStateServiceError;
    type Subscriber = RemoteStateServiceSubscriber;
    type Config = FetchServiceConfig;
    /// Initializes a new StateService instance and starts sync process.
    async fn spawn(config: FetchServiceConfig) -> Result<Self, RemoteStateServiceError> {
        info!("Launching Chain Fetch Service..");

        let rpc_client = JsonRpcConnector::new_from_config_parts(
            config.validator_cookie_auth,
            config.validator_rpc_address,
            config.validator_rpc_user.clone(),
            config.validator_rpc_password.clone(),
            config.validator_cookie_path.clone(),
        )
        .await?;

        let zebra_build_data = rpc_client.get_info().await?;
        let data = ServiceMetadata::new(
            get_build_info(),
            config.network.clone(),
            zebra_build_data.build,
            zebra_build_data.subversion,
        );

        let block_cache = BlockCache::spawn(&rpc_client, config.clone().into()).await?;

        let mempool = Mempool::spawn(&rpc_client, None).await?;

        info!("Launching Remote ReadStateService..");
        let remote_state_service = zebra_state::RemoteStateService::new(
            test_node_and_return_url(
                config.validator_rpc_address,
                config.validator_cookie_auth,
                config.validator_cookie_path.clone(),
                Some(config.validator_rpc_user.clone()),
                Some(config.validator_rpc_password.clone()),
            )
            .await?,
        )?;

        let fetch_service = Self {
            remote_state_service,
            rpc_client,
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
    fn get_subscriber(&self) -> IndexerSubscriber<RemoteStateServiceSubscriber> {
        IndexerSubscriber::new(RemoteStateServiceSubscriber {
            remote_state_service: self.remote_state_service.clone(),
            rpc_client: self.rpc_client.clone(),
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

impl Drop for RemoteStateService {
    fn drop(&mut self) {
        self.close()
    }
}

/// A fetch service subscriber.
///
/// Subscribers should be
#[derive(Debug, Clone)]
pub struct RemoteStateServiceSubscriber {
    /// Remote wrappper functionality for zebra's [`ReadStateService`].
    pub remote_state_service: zebra_state::RemoteStateService,
    /// JsonRPC Client.
    pub rpc_client: JsonRpcConnector,
    /// Local compact block cache.
    pub block_cache: BlockCacheSubscriber,
    /// Internal mempool.
    pub mempool: MempoolSubscriber,
    /// Service metadata.
    pub data: ServiceMetadata,
    /// StateService config data.
    config: FetchServiceConfig,
}

/// Private RPC methods, which are used as helper methods by the public ones
///
/// These would be simple to add to the public interface if
/// needed, there are currently no plans to do so.
impl RemoteStateServiceSubscriber {
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
    /// - `hash_or_height`: (string, required, example="1") The hash or height for the block to be returned.
    /// - `verbose`: (bool, optional, default=false, example=true) false for hex encoded data, true for a json object
    ///
    /// # Notes
    ///
    /// The undocumented `chainwork` field is not returned.
    ///
    /// This rpc is used by get_block(verbose), there is currently no plan to offer this RPC publicly.
    async fn get_block_header(
        &self,
        hash_or_height: String,
        verbose: Option<bool>,
    ) -> Result<GetBlockHeader, RemoteStateServiceError> {
        let mut remote_state = self.remote_state_service.clone();

        let verbose = verbose.unwrap_or(true);
        let network = self.data.network().clone();

        let hash_or_height: HashOrHeight = hash_or_height.parse()?;

        let zebra_state::ReadResponse::BlockHeader {
            header,
            hash,
            height,
            next_block_hash,
        } = remote_state
            .ready()
            .and_then(|service| service.call(zebra_state::ReadRequest::BlockHeader(hash_or_height)))
            .await
            .map_err(|_| {
                RemoteStateServiceError::RpcError(zaino_fetch::jsonrpc::connector::RpcError {
                    // Compatibility with zcashd. Note that since this function
                    // is reused by getblock(), we return the errors expected
                    // by it (they differ whether a hash or a height was passed)
                    code: LegacyCode::InvalidParameter as i64,
                    message: "block height not in best chain".to_string(),
                    data: None,
                })
            })?
        else {
            return Err(RemoteStateServiceError::Custom(
                "Unexpected response to BlockHeader request".to_string(),
            ));
        };

        let response = if !verbose {
            GetBlockHeader::Raw(HexData(header.zcash_serialize_to_vec()?))
        } else {
            let zebra_state::ReadResponse::SaplingTree(sapling_tree) = remote_state
                .ready()
                .and_then(|service| {
                    service.call(zebra_state::ReadRequest::SaplingTree(hash_or_height))
                })
                .await?
            else {
                return Err(RemoteStateServiceError::Custom(
                    "Unexpected response to SaplingTree request".to_string(),
                ));
            };

            // This could be `None` if there's a chain reorg between state queries.
            let sapling_tree = sapling_tree.ok_or_else(|| {
                RemoteStateServiceError::RpcError(zaino_fetch::jsonrpc::connector::RpcError {
                    code: LegacyCode::InvalidParameter as i64,
                    message: "missing sapling tree for block".to_string(),
                    data: None,
                })
            })?;

            let zebra_state::ReadResponse::Depth(depth) = remote_state
                .ready()
                .and_then(|service| service.call(zebra_state::ReadRequest::Depth(hash)))
                .await?
            else {
                return Err(RemoteStateServiceError::Custom(
                    "Unexpected response to Depth request".to_string(),
                ));
            };

            // From <https://zcash.github.io/rpc/getblock.html>
            // TODO: Deduplicate const definition, consider refactoring this to avoid duplicate logic
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
            let block_commitments = header_to_block_commitments(
                &header,
                &self.config.network,
                height,
                final_sapling_root,
            )?;

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
}

#[async_trait]
impl ZcashIndexer for RemoteStateServiceSubscriber {
    type Error = RemoteStateServiceError;

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
        let mut remote_state = self.remote_state_service.clone();

        let response = remote_state
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

        let usage_response = remote_state
            .ready()
            .and_then(|service| service.call(ReadRequest::UsageInfo))
            .await?;
        let size_on_disk = expected_read_response!(usage_response, UsageInfo);

        let request = zebra_state::ReadRequest::BlockHeader(hash.into());
        let response = remote_state
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
                    // All the network upgrades with a consensus branch ID are the same in Zebra and zcashd.
                    network_upgrade.branch_id().map(|branch_id| {
                        // zcashd's RPC seems to ignore Disabled network upgrades, so Zebra does too.
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

        let difficulty = if self.config.network.clone().is_regtest() {
            0.0
        } else {
            let response = dbg!(
                remote_state
                    .ready()
                    .and_then(|service| service.call(ReadRequest::ChainInfo))
                    .await
            )?;
            let chain_info = match response {
                ReadResponse::ChainInfo(info) => info,
                _ => unreachable!("unmatched response to a chain info request"),
            };
            // // Shift out the lower 128 bits (256 bits, but the top 128 are all zeroes)
            let difficulty = chain_info
                .expected_difficulty
                .to_expanded()
                .map(|expanded| zebra_chain::work::difficulty::U256::from(expanded) >> 128)
                .unwrap_or(zebra_chain::work::difficulty::U256::zero());
            // Convert to u128 then f64.
            // We could also convert U256 to String, then parse as f64, but that's slower.
            let difficulty = difficulty.as_u128() as f64;
            difficulty
        };

        let verification_progress = f64::from(height.0) / f64::from(zebra_estimated_height.0);

        Ok(GetBlockChainInfo::new(
            self.config.network.bip70_network_name(),
            height,
            hash,
            estimated_height,
            zebra_rpc::methods::types::Balance::chain_supply(balance),
            zebra_rpc::methods::types::Balance::value_pools(balance),
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
        let mut remote_state = self.remote_state_service.clone();

        let strings_set = address_strings
            .valid_addresses()
            .map_err(|e| RpcError::new_from_errorobject(e, "invalid taddrs provided"))?;

        let response = remote_state
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
        // Offload to the json rpc connector, as ReadStateService doesn't yet interface with the mempool
        self.rpc_client
            .send_raw_transaction(raw_transaction_hex)
            .await
            .map(SentTransactionHash::from)
            .map_err(RemoteStateServiceError::JsonRpcConnectorError)
    }

    async fn z_get_block(
        &self,
        hash_or_height_string: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        let mut remote_state_1 = self.remote_state_service.clone();
        let remote_state_2 = self.remote_state_service.clone();

        let verbosity = verbosity.unwrap_or(1);
        let hash_or_height = HashOrHeight::from_str(&hash_or_height_string);
        match verbosity {
            0 => {
                let request = ReadRequest::Block(hash_or_height?);
                let response = remote_state_1
                    .ready()
                    .and_then(|service| service.call(request))
                    .await?;
                let block = expected_read_response!(response, Block);
                block.map(SerializedBlock::from).map(GetBlock::Raw).ok_or(
                    RemoteStateServiceError::RpcError(RpcError::new_from_legacycode(
                        LegacyCode::InvalidParameter,
                        "block not found",
                    )),
                )
            }
            1 | 2 => {
                let hash_or_height = hash_or_height?;
                let txids_or_fullblock_request = match verbosity {
                    1 => ReadRequest::TransactionIdsForBlock(hash_or_height),
                    2 => ReadRequest::Block(hash_or_height),
                    _ => unreachable!("verbosity is known to be 1 or 2"),
                };

                let txids_future = {
                    let mut rs = remote_state_1;
                    let req = txids_or_fullblock_request;
                    async move { rs.ready().and_then(|service| service.call(req)).await }
                };

                let orchard_future = {
                    let mut rs2 = remote_state_2;
                    let req = ReadRequest::OrchardTree(hash_or_height);
                    async move { rs2.ready().and_then(|service| service.call(req)).await }
                };

                let (txids_or_fullblock, orchard_tree_response, header) = futures::join!(
                    txids_future,
                    orchard_future,
                    self.get_block_header(hash_or_height_string, Some(true))
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
                            GetBlockTransaction::Object(TransactionObject {
                                hex: transaction.as_ref().into(),
                                height: Some(height.0),
                                // Confirmations should never be greater than the current block height
                                confirmations: Some(confirmations as u32),
                            })
                        })
                        .collect()),
                    Ok(ReadResponse::TransactionIdsForBlock(None))
                    | Ok(ReadResponse::Block(None)) => Err(RemoteStateServiceError::RpcError(
                        RpcError::new_from_legacycode(
                            LegacyCode::InvalidParameter,
                            "block not found",
                        ),
                    )),
                    Ok(unexpected) => {
                        unreachable!("Unexpected response from state service: {unexpected:?}")
                    }
                    Err(e) => Err(e.into()),
                }?;

                let orchard_tree_response = orchard_tree_response?;
                let orchard_tree = expected_read_response!(orchard_tree_response, OrchardTree)
                    .ok_or(RemoteStateServiceError::RpcError(
                        RpcError::new_from_legacycode(LegacyCode::Misc, "missing orchard tree"),
                    ))?;

                let final_orchard_root =
                    match NetworkUpgrade::Nu5.activation_height(&self.config.network) {
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
            more_than_two => Err(RemoteStateServiceError::RpcError(
                RpcError::new_from_legacycode(
                    LegacyCode::InvalidParameter,
                    format!("invalid verbosity of {more_than_two}"),
                ),
            )),
        }
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
        let mut remote_state = self.remote_state_service.clone();

        let hash_or_height = HashOrHeight::from_str(&hash_or_height)?;
        let block_header_response = remote_state
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
                remote_state
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
                remote_state
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

    async fn z_get_subtrees_by_index(
        &self,
        pool: String,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<GetSubtrees, Self::Error> {
        let mut remote_state = self.remote_state_service.clone();

        match pool.as_str() {
            "sapling" => {
                let request = zebra_state::ReadRequest::SaplingSubtrees { start_index, limit };
                let response = remote_state
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
                let response = remote_state
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
            otherwise => Err(RemoteStateServiceError::RpcError(
                RpcError::new_from_legacycode(
                    LegacyCode::Misc,
                    format!(
                        "invalid pool name \"{otherwise}\", must be \"sapling\" or \"orchard\""
                    ),
                ),
            )),
        }
    }

    async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetRawTransaction, Self::Error> {
        let mut remote_state = self.remote_state_service.clone();

        let txid = zebra_chain::transaction::Hash::from_hex(txid_hex).map_err(|e| {
            RpcError::new_from_legacycode(LegacyCode::InvalidAddressOrKey, e.to_string())
        })?;

        // check the mempool for the transaction
        let mempool_transaction_future = self.rpc_client.get_raw_mempool().then(|result| async {
            result.map(|TxidsResponse { transactions }| {
                transactions
                    .into_iter()
                    .find(|mempool_txid| *mempool_txid == txid.to_string())
            })
        });
        let onchain_transaction_future = remote_state
            .ready()
            .and_then(|service| service.call(ReadRequest::Transaction(txid)));

        futures::pin_mut!(mempool_transaction_future);
        futures::pin_mut!(onchain_transaction_future);

        // This might be overengineered...try to find the txid on chain and in the mempool,
        // whichever one resolves first is tried first.
        let resolution =
            futures::future::select(mempool_transaction_future, onchain_transaction_future).await;

        let handle_mempool = |txid| async {
            self.rpc_client
                .get_raw_transaction(txid, verbose)
                .await
                .map(GetRawTransaction::from)
                .map_err(RemoteStateServiceError::JsonRpcConnectorError)
        };

        let handle_onchain = |response| {
            let transaction = expected_read_response!(response, Transaction);
            match transaction {
                Some(MinedTx {
                    tx,
                    height,
                    confirmations,
                }) => Ok(match verbose {
                    Some(_verbosity) => GetRawTransaction::Object(TransactionObject {
                        hex: tx.into(),
                        height: Some(height.0),
                        confirmations: Some(confirmations),
                    }),
                    None => GetRawTransaction::Raw(tx.into()),
                }),
                None => Err(RemoteStateServiceError::RpcError(
                    RpcError::new_from_legacycode(
                        LegacyCode::InvalidAddressOrKey,
                        "No such mempool or main chain transaction",
                    ),
                )),
            }
        };

        match resolution {
            futures::future::Either::Left((response, other_fut)) => match response? {
                Some(txid) => handle_mempool(txid).await,
                None => {
                    let response = other_fut.await?;
                    handle_onchain(response)
                }
            },
            futures::future::Either::Right((response, other_fut)) => {
                match handle_onchain(response?) {
                    Ok(val) => Ok(val),
                    Err(e) => match other_fut.await? {
                        Some(txid) => handle_mempool(txid).await,
                        None => Err(e),
                    },
                }
            }
        }
    }

    async fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, Self::Error> {
        let mut remote_state = self.remote_state_service.clone();

        let (addresses, start, end) = request.into_parts();

        let response = remote_state
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
            "start {start:?} and end {end:?} must both be less than or equal to the chain tip {chain_height:?}")
            );
        }

        if let Some(e) = error_string {
            return Err(RemoteStateServiceError::RpcError(
                RpcError::new_from_legacycode(LegacyCode::InvalidParameter, e),
            ));
        }

        let request = ReadRequest::TransactionIdsByAddresses {
            addresses: AddressStrings::new_valid(addresses)
                .and_then(|addrs| addrs.valid_addresses())
                .map_err(|e| RpcError::new_from_errorobject(e, "invalid adddress"))?,

            height_range: Height(start)..=Height(end),
        };

        let response = remote_state
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
        let mut remote_state = self.remote_state_service.clone();

        let valid_addresses = address_strings
            .valid_addresses()
            .map_err(|e| RpcError::new_from_errorobject(e, "invalid address"))?;
        let request = ReadRequest::UtxosByAddresses(valid_addresses);
        let response = remote_state
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
}

#[async_trait]
impl LightWalletIndexer for RemoteStateServiceSubscriber {
    type Error = RemoteStateServiceError;

    /// Return the height of the tip of the best chain
    async fn get_latest_block(&self) -> Result<BlockId, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// Return the compact block corresponding to the given block identifier
    async fn get_block(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        let height: u32 = match request.height.try_into() {
            Ok(height) => height,
            Err(_) => {
                return Err(RemoteStateServiceError::TonicStatusError(
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
                    Err(RemoteStateServiceError::TonicStatusError(tonic::Status::out_of_range(
                            format!(
                                "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                                height, chain_height,
                            )
                        )))
                } else {
                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                    Err(RemoteStateServiceError::TonicStatusError(
                        tonic::Status::unknown(format!(
                            "Error: Failed to retrieve block from node. Server Error: {}",
                            e,
                        )),
                    ))
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
                return Err(RemoteStateServiceError::TonicStatusError(
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
                    Err(RemoteStateServiceError::TonicStatusError(tonic::Status::out_of_range(
                            format!(
                                "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                                height, chain_height,
                            )
                        )))
                } else {
                    // TODO: Hide server error from clients before release. Currently useful for dev purposes.
                    Err(RemoteStateServiceError::TonicStatusError(
                        tonic::Status::unknown(format!(
                            "Error: Failed to retrieve block from node. Server Error: {}",
                            e,
                        )),
                    ))
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
                    return Err(RemoteStateServiceError::TonicStatusError(
                        tonic::Status::invalid_argument(
                            "Error: Start height out of range. Failed to convert to u32.",
                        ),
                    ));
                }
            },
            None => {
                return Err(RemoteStateServiceError::TonicStatusError(
                    tonic::Status::invalid_argument("Error: No start height given."),
                ));
            }
        };
        let mut end: u32 = match request.end {
            Some(block_id) => match block_id.height.try_into() {
                Ok(height) => height,
                Err(_) => {
                    return Err(RemoteStateServiceError::TonicStatusError(
                        tonic::Status::invalid_argument(
                            "Error: End height out of range. Failed to convert to u32.",
                        ),
                    ));
                }
            },
            None => {
                return Err(RemoteStateServiceError::TonicStatusError(
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
            |err| RemoteStateServiceError::TonicStatusError(tonic::Status::invalid_argument(err));
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
    async fn get_transaction(&self, _request: TxFilter) -> Result<RawTransaction, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
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
        _request: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// Returns the total balance for a list of taddrs
    async fn get_taddress_balance(
        &self,
        _request: AddressList,
    ) -> Result<zaino_proto::proto::service::Balance, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// Returns the total balance for a list of taddrs
    async fn get_taddress_balance_stream(
        &self,
        _request: AddressStream,
    ) -> Result<zaino_proto::proto::service::Balance, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
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
    async fn get_tree_state(&self, _request: BlockId) -> Result<TreeState, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// GetLatestTreeState returns the note commitment tree state corresponding to the chain tip.
    async fn get_latest_tree_state(&self) -> Result<TreeState, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// Returns a stream of information about roots of subtrees of the Sapling and Orchard
    /// note commitment trees.
    async fn get_subtree_roots(
        &self,
        _request: GetSubtreeRootsArg,
    ) -> Result<SubtreeRootReplyStream, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are collected and returned as a single Vec.
    async fn get_address_utxos(
        &self,
        _request: GetAddressUtxosArg,
    ) -> Result<GetAddressUtxosReplyList, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are returned in a stream.
    async fn get_address_utxos_stream(
        &self,
        _request: GetAddressUtxosArg,
    ) -> Result<UtxoReplyStream, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// Return information about this lightwalletd instance and the blockchain
    async fn get_lightd_info(&self) -> Result<LightdInfo, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }

    /// Testing-only, requires lightwalletd --ping-very-insecure (do not enable in production)
    ///
    /// NOTE: Currently unimplemented in Zaino.
    async fn ping(&self, _request: Duration) -> Result<PingResponse, Self::Error> {
        Err(crate::error::RemoteStateServiceError::TonicStatusError(tonic::Status::unimplemented(
            "Ping not yet implemented. If you require this RPC please open an issue or PR at the Zaino github (https://github.com/zingolabs/zaino.git)."
        )))
    }
}
