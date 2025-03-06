//! Zcash chain fetch and tx submission service backed by Zebras [`ReadStateService`].

use crate::{
    config::StateServiceConfig,
    error::StateServiceError,
    indexer::ZcashIndexer,
    status::{AtomicStatus, StatusType},
    stream::CompactBlockStream,
    utils::{get_build_info, ServiceMetadata},
};

use chrono::Utc;
use futures::FutureExt as _;
use hex::{FromHex as _, ToHex as _};
use indexmap::IndexMap;
use std::future::poll_fn;
use std::io::Cursor;
use std::str::FromStr as _;
use tokio::time::timeout;
use tonic::async_trait;
use tower::Service;

use zaino_fetch::jsonrpc::connector::{JsonRpcConnector, RpcError};
use zaino_fetch::jsonrpc::response::TxidsResponse;
use zaino_proto::proto::compact_formats::ChainMetadata;
use zaino_proto::proto::compact_formats::{
    CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTx,
};
use zaino_proto::proto::service::BlockRange;

use zebra_chain::{
    block::{Header, Height, SerializedBlock},
    chain_tip::{ChainTip, NetworkChainTipHeightEstimator},
    parameters::{ConsensusBranchId, Network, NetworkUpgrade},
    serialization::{ZcashDeserialize, ZcashSerialize},
    subtree::NoteCommitmentSubtreeIndex,
    transaction::Transaction,
};
use zebra_rpc::{
    methods::{
        chain_tip_difficulty,
        hex_data::HexData,
        trees::{GetSubtrees, GetTreestate, SubtreeRpcData},
        types::Balance,
        AddressBalance, AddressStrings, ConsensusBranchIdHex, GetAddressTxIdsRequest,
        GetAddressUtxos, GetBlock, GetBlockChainInfo, GetBlockHash, GetBlockHeader,
        GetBlockHeaderObject, GetBlockTransaction, GetBlockTrees, GetInfo, GetRawTransaction,
        NetworkUpgradeInfo, NetworkUpgradeStatus, SentTransactionHash, TipConsensusBranch,
        TransactionObject,
    },
    server::error::LegacyCode,
    sync::init_read_state_with_syncer,
};
use zebra_state::{
    ChainTipChange, HashOrHeight, LatestChainTip, MinedTx, OutputLocation, ReadRequest,
    ReadResponse, ReadStateService, TransactionLocation,
};

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
/// NOTE: We currently dop not implement clone for chain fetch services as this service is responsible for maintaining and closing its child processes.
///       ServiceSubscribers are used to create separate chain fetch processes while allowing central state processes to be managed in a single place.
///       If we want the ability to clone Service all JoinHandle's should be converted to Arc<JoinHandle>.
#[derive(Debug)]
pub struct StateService {
    /// `ReadeStateService` from Zebra-State.
    read_state_service: ReadStateService,
    /// Tracks the latest chain tip.
    latest_chain_tip: LatestChainTip,
    /// Monitors changes in the chain tip.
    _chain_tip_change: ChainTipChange,
    /// Sync task handle.
    sync_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// JsonRPC Client.
    rpc_client: JsonRpcConnector,
    /// Service metadata.
    data: ServiceMetadata,
    /// StateService config data.
    config: StateServiceConfig,
    /// Thread-safe status indicator.
    status: AtomicStatus,
}

impl StateService {
    /// Initializes a new StateService instance and starts sync process.
    pub async fn spawn(config: StateServiceConfig) -> Result<Self, StateServiceError> {
        let rpc_client = JsonRpcConnector::new_from_config_parts(
            config.validator_cookie_auth,
            config.validator_rpc_address,
            config.validator_rpc_user.clone(),
            config.validator_rpc_password.clone(),
            config.validator_cookie_path.clone(),
        )
        .await?;

        let (read_state_service, latest_chain_tip, chain_tip_change, sync_task_handle) =
            init_read_state_with_syncer(
                config.validator_config.clone(),
                &config.network,
                config.validator_rpc_address,
            )
            .await??;

        let zebra_build_data = rpc_client.get_info().await?;

        let data = ServiceMetadata::new(
            get_build_info(),
            config.network.clone(),
            zebra_build_data.build,
            zebra_build_data.subversion,
        );

        let mut state_service = Self {
            read_state_service,
            latest_chain_tip,
            _chain_tip_change: chain_tip_change,
            sync_task_handle: Some(sync_task_handle),
            rpc_client,
            data,
            config,
            status: AtomicStatus::new(StatusType::Spawning.into()),
        };

        state_service.status.store(StatusType::Syncing.into());

        // TODO: Update initial sync to use latest_chain_tip, this should be done once zebra regtest is running rug free.
        poll_fn(|cx| state_service.read_state_service.poll_ready(cx)).await?;

        state_service.status.store(StatusType::Ready.into());

        Ok(state_service)
    }

    /// A combined function that checks readiness using `poll_ready` and then performs the request.
    /// If the service is busy, it waits until ready. If there's an error, it returns the error.
    pub(crate) async fn checked_call(
        &self,
        req: zebra_state::ReadRequest,
    ) -> Result<zebra_state::ReadResponse, StateServiceError> {
        let mut read_state_service = self.read_state_service.clone();
        poll_fn(|cx| read_state_service.poll_ready(cx)).await?;
        read_state_service
            .call(req)
            .await
            .map_err(StateServiceError::from)
    }

    /// A combined function that checks readiness using `poll_ready` and then performs the request.
    /// If the service is busy, it waits until ready. If there's an error, it returns the error.
    ///
    /// Avoides taking `Self`.
    pub(crate) async fn checked_call_decoupled(
        mut read_state_service: ReadStateService,
        req: zebra_state::ReadRequest,
    ) -> Result<zebra_state::ReadResponse, StateServiceError> {
        poll_fn(|cx| read_state_service.poll_ready(cx)).await?;
        read_state_service
            .call(req)
            .await
            .map_err(StateServiceError::from)
    }

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

    /// Returns the StateService's Status.
    ///
    /// We first check for `status = StatusType::Closing` as this signifies a shutdown order from an external process.
    pub async fn status(&self) -> StatusType {
        let current_status = self.status.load().into();
        if current_status == StatusType::Closing {
            current_status
        } else {
            self.fetch_status_from_validator().await
        }
    }

    /// Shuts down the StateService.
    pub fn close(&mut self) {
        if self.sync_task_handle.is_some() {
            if let Some(handle) = self.sync_task_handle.take() {
                handle.abort();
            }
        }
    }
}

#[async_trait]
impl ZcashIndexer for StateService {
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
        let response = self.checked_call(ReadRequest::TipPoolValues).await?;
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
        let usage_response = self.checked_call(ReadRequest::UsageInfo).await?;
        let size_on_disk = expected_read_response!(usage_response, UsageInfo);
        let request = zebra_state::ReadRequest::BlockHeader(hash.into());
        let response = self.checked_call(request).await?;
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
        let difficulty =
            chain_tip_difficulty(self.config.network.clone(), self.read_state_service.clone())
                .await
                .unwrap();

        let verification_progress = f64::from(height.0) / f64::from(zebra_estimated_height.0);

        Ok(GetBlockChainInfo::new(
            self.config.network.bip70_network_name(),
            height,
            hash,
            estimated_height,
            Balance::chain_supply(balance),
            Balance::value_pools(balance),
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
        let strings_set = address_strings
            .valid_addresses()
            .map_err(|e| RpcError::new_from_errorobject(e, "invalid taddrs provided"))?;
        let response = self
            .checked_call(ReadRequest::AddressBalance(strings_set))
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
            .map_err(StateServiceError::JsonRpcConnectorError)
    }

    async fn z_get_block(
        &self,
        hash_or_height_string: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        let verbosity = verbosity.unwrap_or(1);
        let hash_or_height = HashOrHeight::from_str(&hash_or_height_string);
        match verbosity {
            0 => {
                let request = ReadRequest::Block(hash_or_height?);
                let response = self.checked_call(request).await?;
                let block = expected_read_response!(response, Block);
                block.map(SerializedBlock::from).map(GetBlock::Raw).ok_or(
                    StateServiceError::RpcError(RpcError::new_from_legacycode(
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

                let (txids_or_fullblock, orchard_tree_response, header) = futures::join!(
                    self.checked_call(txids_or_fullblock_request),
                    self.checked_call(ReadRequest::OrchardTree(hash_or_height)),
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
                    | Ok(ReadResponse::Block(None)) => {
                        Err(StateServiceError::RpcError(RpcError::new_from_legacycode(
                            LegacyCode::InvalidParameter,
                            "block not found",
                        )))
                    }
                    Ok(unexpected) => {
                        unreachable!("Unexpected response from state service: {unexpected:?}")
                    }
                    Err(e) => Err(e),
                }?;

                let orchard_tree_response = orchard_tree_response?;
                let orchard_tree = expected_read_response!(orchard_tree_response, OrchardTree)
                    .ok_or(StateServiceError::RpcError(RpcError::new_from_legacycode(
                        LegacyCode::Misc,
                        "missing orchard tree",
                    )))?;

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
            more_than_two => Err(StateServiceError::RpcError(RpcError::new_from_legacycode(
                LegacyCode::InvalidParameter,
                format!("invalid verbosity of {more_than_two}"),
            ))),
        }
    }

    async fn get_raw_mempool(&self) -> Result<Vec<String>, Self::Error> {
        let txids = self.rpc_client.get_raw_mempool().await?;
        Ok(txids.transactions)
    }

    async fn z_get_treestate(&self, hash_or_height: String) -> Result<GetTreestate, Self::Error> {
        let hash_or_height = HashOrHeight::from_str(&hash_or_height)?;
        let block_header_response = self
            .checked_call(ReadRequest::BlockHeader(hash_or_height))
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
                self.checked_call(ReadRequest::SaplingTree(hash_or_height))
                    .await?,
            ),
            _ => None,
        }
        .and_then(|sap_response| {
            expected_read_response!(sap_response, SaplingTree).map(|tree| tree.to_rpc_bytes())
        });
        let orchard = match NetworkUpgrade::Nu5.activation_height(&self.config.network) {
            Some(activation_height) if height >= activation_height => Some(
                self.checked_call(ReadRequest::OrchardTree(hash_or_height))
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
        match pool.as_str() {
            "sapling" => {
                let request = zebra_state::ReadRequest::SaplingSubtrees { start_index, limit };
                let response = self.checked_call(request).await?;
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
                let response = self.checked_call(request).await?;
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
        let onchain_transaction_future = self.checked_call(ReadRequest::Transaction(txid));

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
                .map_err(StateServiceError::JsonRpcConnectorError)
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
                None => Err(StateServiceError::RpcError(RpcError::new_from_legacycode(
                    LegacyCode::InvalidAddressOrKey,
                    "No such mempool or main chain transaction",
                ))),
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
        let (addresses, start, end) = request.into_parts();
        let chain_height =
            self.latest_chain_tip
                .best_tip_height()
                .ok_or(RpcError::new_from_legacycode(
                    LegacyCode::Misc,
                    "no blocks in chain",
                ))?;

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

        let response = self.checked_call(request).await?;

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
        let valid_addresses = address_strings
            .valid_addresses()
            .map_err(|e| RpcError::new_from_errorobject(e, "invalid address"))?;
        let request = ReadRequest::UtxosByAddresses(valid_addresses);
        let response = self.checked_call(request).await?;
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

impl Drop for StateService {
    fn drop(&mut self) {
        if let Some(handle) = self.sync_task_handle.take() {
            handle.abort();
        }
    }
}

/// Private RPC methods, which are used as helper methods by the public ones
///
/// These would be simple to add to the public interface if
/// needed, there are currently no plans to do so.
impl StateService {
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
    ) -> Result<GetBlockHeader, StateServiceError> {
        let verbose = verbose.unwrap_or(true);
        let network = self.data.network().clone();

        let hash_or_height: HashOrHeight = hash_or_height.parse()?;

        let zebra_state::ReadResponse::BlockHeader {
            header,
            hash,
            height,
            next_block_hash,
        } = self
            .checked_call(zebra_state::ReadRequest::BlockHeader(hash_or_height))
            .await
            .map_err(|_| {
                StateServiceError::RpcError(zaino_fetch::jsonrpc::connector::RpcError {
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
            let zebra_state::ReadResponse::SaplingTree(sapling_tree) = self
                .checked_call(zebra_state::ReadRequest::SaplingTree(hash_or_height))
                .await?
            else {
                return Err(StateServiceError::Custom(
                    "Unexpected response to SaplingTree request".to_string(),
                ));
            };

            // This could be `None` if there's a chain reorg between state queries.
            let sapling_tree = sapling_tree.ok_or_else(|| {
                StateServiceError::RpcError(zaino_fetch::jsonrpc::connector::RpcError {
                    code: LegacyCode::InvalidParameter as i64,
                    message: "missing sapling tree for block".to_string(),
                    data: None,
                })
            })?;

            let zebra_state::ReadResponse::Depth(depth) = self
                .checked_call(zebra_state::ReadRequest::Depth(hash))
                .await?
            else {
                return Err(StateServiceError::Custom(
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

    /// Returns a [`zaino_proto::proto::compact_formats::CompactTx`].
    ///
    /// Notes:
    ///
    /// Written to avoid taking [`Self`] to simplify use in [`get_block_range`].
    ///
    /// This function is used by get_block_range, there is currently no plan to offer this RPC publicly.
    ///
    /// LightWalletD doesnt return a compact block header, however this could be used to return data if useful.
    ///
    /// This impl is still slow, either CompactBl,ocks should be returned directly from the [`ReadStateService`] or Zaino should hold an internal compact block cache.
    async fn get_compact_block(
        read_state_service: &ReadStateService,
        hash_or_height: String,
        network: &Network,
    ) -> Result<CompactBlock, StateServiceError> {
        let hash_or_height: HashOrHeight = hash_or_height.parse()?;
        let cloned_read_state_service = read_state_service.clone();
        let cloned_network = network.clone();
        let get_block_header_future = tokio::spawn(async move {
            let zebra_state::ReadResponse::BlockHeader {
                header,
                hash,
                height,
                next_block_hash,
            } = StateService::checked_call_decoupled(
                cloned_read_state_service.clone(),
                zebra_state::ReadRequest::BlockHeader(hash_or_height),
            )
            .await
            .map_err(|_| {
                StateServiceError::RpcError(zaino_fetch::jsonrpc::connector::RpcError {
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

            let zebra_state::ReadResponse::SaplingTree(sapling_tree) =
                StateService::checked_call_decoupled(
                    cloned_read_state_service.clone(),
                    zebra_state::ReadRequest::SaplingTree(hash_or_height),
                )
                .await?
            else {
                return Err(StateServiceError::Custom(
                    "Unexpected response to SaplingTree request".to_string(),
                ));
            };

            // This could be `None` if there's a chain reorg between state queries.
            let sapling_tree = sapling_tree.ok_or_else(|| {
                StateServiceError::RpcError(zaino_fetch::jsonrpc::connector::RpcError {
                    code: LegacyCode::InvalidParameter as i64,
                    message: "missing sapling tree for block".to_string(),
                    data: None,
                })
            })?;

            let zebra_state::ReadResponse::Depth(depth) = StateService::checked_call_decoupled(
                cloned_read_state_service,
                zebra_state::ReadRequest::Depth(hash),
            )
            .await?
            else {
                return Err(StateServiceError::Custom(
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

            let sapling_activation = NetworkUpgrade::Sapling.activation_height(&cloned_network);
            let sapling_tree_size = sapling_tree.count();
            let final_sapling_root: [u8; 32] =
                if sapling_activation.is_some() && height >= sapling_activation.unwrap() {
                    let mut root: [u8; 32] = sapling_tree.root().into();
                    root.reverse();
                    root
                } else {
                    [0; 32]
                };

            let difficulty = header
                .difficulty_threshold
                .relative_to_network(&cloned_network);
            let block_commitments =
                header_to_block_commitments(&header, &cloned_network, height, final_sapling_root)?;

            Ok(GetBlockHeaderObject {
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
            })
        });

        let get_orchard_trees_future = StateService::checked_call_decoupled(
            read_state_service.clone(),
            zebra_state::ReadRequest::OrchardTree(hash_or_height),
        );

        let zebra_state::ReadResponse::Block(Some(block_raw)) =
            StateService::checked_call_decoupled(
                read_state_service.clone(),
                zebra_state::ReadRequest::Block(hash_or_height),
            )
            .await?
        else {
            return Err(StateServiceError::RpcError(
                zaino_fetch::jsonrpc::connector::RpcError {
                    code: LegacyCode::InvalidParameter as i64,
                    message: "Block not found".to_string(),
                    data: None,
                },
            ));
        };

        let block_bytes = block_raw.zcash_serialize_to_vec().map_err(|e| {
            StateServiceError::Custom(format!("Failed to serialize block: {:#?}", e))
        })?;
        let mut cursor = Cursor::new(block_bytes);
        let block = zebra_chain::block::Block::zcash_deserialize(&mut cursor).map_err(|e| {
            StateServiceError::Custom(format!("Failed to deserialize block bytes: {:#?}", e))
        })?;
        let vtx = block
            .transactions
            .into_iter()
            .enumerate()
            .filter_map(|(index, tx)| {
                if tx.has_shielded_inputs() || tx.has_shielded_outputs() {
                    Some(tx_to_compact(tx, index as u64))
                } else {
                    None
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        let block_header = get_block_header_future.await??;
        let zebra_state::ReadResponse::OrchardTree(Some(orchard_trees)) =
            get_orchard_trees_future.await?
        else {
            return Err(StateServiceError::Custom(
                "Unexpected response type for OrchardTrees".into(),
            ));
        };
        let chain_metadata = Some(ChainMetadata {
            sapling_commitment_tree_size: block_header.sapling_tree_size.try_into()?,
            orchard_commitment_tree_size: orchard_trees.count().try_into()?,
        });

        let compact_block = CompactBlock {
            proto_version: block.header.version,
            height: block_header.height.0 as u64,
            hash: block_header.hash.0 .0.to_vec(),
            prev_hash: block_header.previous_block_hash.0 .0.to_vec(),
            time: block_header.time.try_into()?,
            header: Vec::new(),
            vtx,
            chain_metadata,
        };

        Ok(compact_block)
    }
}

/// This impl will hold the Lightwallet RPC method implementations for StateService.
///
/// TODO: Update this to be `impl LightWalletIndexer for StateService` once rpc methods are implemented and tested (or implement separately).
impl StateService {
    /// Return a list of consecutive compact blocks.
    pub async fn get_block_range(
        &self,
        blockrange: BlockRange,
    ) -> Result<CompactBlockStream, StateServiceError> {
        let mut start: u32 = match blockrange.start {
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
        let mut end: u32 = match blockrange.end {
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
        let rev_order = if start > end {
            (start, end) = (end, start);
            true
        } else {
            false
        };

        let cloned_read_state_service = self.read_state_service.clone();
        let network = self.config.network.clone();
        let service_channel_size = self.config.service_channel_size;
        let service_timeout = self.config.service_timeout;
        let latest_chain_tip = self.latest_chain_tip.clone();
        let (channel_tx, channel_rx) = tokio::sync::mpsc::channel(service_channel_size as usize);
        tokio::spawn(async move {
            let timeout = timeout(
                std::time::Duration::from_secs(service_timeout as u64),
                async {
                    for height in start..=end {
                        let height = if rev_order {
                            end - (height - start)
                        } else {
                            height
                        };
                        match StateService::get_compact_block(
                            &cloned_read_state_service,
                            height.to_string(),
                            &network,
                        ).await {
                            Ok(block) => {
                                if channel_tx.send(Ok(block)).await.is_err() {
                                    break;
                                };
                            }
                            Err(e) => {
                                let chain_height = match latest_chain_tip.best_tip_height() {
                                    Some(ch) => ch.0,
                                    None => {
                                    if let Err(e) = channel_tx
                                        .send(Err(tonic::Status::unknown("No best tip height found")))
                                        .await
                                        {
                                            eprintln!("Error: channel closed unexpectedly: {e}");
                                        }
                                    break;
                                    }
                                };
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
                                            eprintln!("Error: Channel closed unexpectedly: {}", e);
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
}

/// Converts a [`zebra_chain::transaction::Transaction`] into a [`zaino_proto::proto::compact_formats::CompactTx`].
///
/// Notes:
///
/// Currently only supports V4 and V5 transactions.
///
/// LightWalletD currently does not return a fee and is not currently priority here. Please open an Issue or PR at the Zingo-Indexer github (https://github.com/zingolabs/zingo-indexer) if you require this functionality.
fn tx_to_compact(
    transaction: std::sync::Arc<Transaction>,
    index: u64,
) -> Result<CompactTx, StateServiceError> {
    let (spends, outputs) = if transaction.has_sapling_shielded_data() {
        (
            transaction
                .sapling_nullifiers()
                .map(|nullifier| CompactSaplingSpend {
                    nf: nullifier.0.to_vec(),
                })
                .collect(),
            transaction
                .sapling_outputs()
                .map(
                    |output| -> Result<CompactSaplingOutput, StateServiceError> {
                        let ciphertext = output
                            .enc_ciphertext
                            .zcash_serialize_to_vec()
                            .map_err(StateServiceError::IoError)?;

                        Ok(CompactSaplingOutput {
                            cmu: output.cm_u.to_bytes().to_vec(),
                            ephemeral_key: <[u8; 32]>::from(output.ephemeral_key).to_vec(),
                            ciphertext,
                        })
                    },
                )
                .collect::<Result<Vec<CompactSaplingOutput>, _>>()?,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let actions = if transaction.has_orchard_shielded_data() {
        transaction
            .orchard_actions()
            .map(
                |action| -> Result<CompactOrchardAction, StateServiceError> {
                    let ciphertext = action
                        .enc_ciphertext
                        .zcash_serialize_to_vec()
                        .map_err(StateServiceError::IoError)?;

                    Ok(CompactOrchardAction {
                        nullifier: <[u8; 32]>::from(action.nullifier).to_vec(),
                        cmx: <[u8; 32]>::from(action.cm_x).to_vec(),
                        ephemeral_key: <[u8; 32]>::from(action.ephemeral_key).to_vec(),
                        ciphertext,
                    })
                },
            )
            .collect::<Result<Vec<CompactOrchardAction>, _>>()?
    } else {
        Vec::new()
    };

    Ok(CompactTx {
        index,
        hash: transaction.hash().0.to_vec(),
        fee: 0,
        spends,
        outputs,
        actions,
    })
}

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
