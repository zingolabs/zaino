//! validator connected blockchain source.

use std::collections::HashMap;
use std::str::FromStr as _;

use chrono::{DateTime, Utc};
use futures::future::join3;
use hex::{FromHex as _, ToHex as _};
use indexmap::IndexMap;
use tracing::info;
use zaino_fetch::jsonrpsee::response::{
    address_deltas::BlockInfo,
    block_deltas::{BlockDelta, BlockDeltas, InputDelta, OutputDelta},
    block_header::GetBlockHeader,
    block_subsidy::GetBlockSubsidy,
    mining_info::GetMiningInfoWire,
    peer_info::GetPeerInfo,
    GetInfoResponse, GetNetworkSolPsResponse, GetSpentInfoRequest, GetSpentInfoResponse,
    GetTxOutResponse,
};
use zebra_rpc::sync::init_read_state_with_syncer;

use crate::config::{CommonBackendConfig, DirectConnectionConfig};
use zebra_chain::{
    amount::{Amount, NonNegative},
    block::{Header, SerializedBlock},
    chain_tip::NetworkChainTipHeightEstimator,
    parameters::{ConsensusBranchId, NetworkUpgrade},
    serialization::{BytesInDisplayOrder as _, ZcashSerialize as _},
};
use zebra_rpc::{
    client::{BlockObject, GetBlockchainInfoBalance, HexData, Input, TransactionObject},
    methods::{
        chain_tip_difficulty, ConsensusBranchIdHex, GetBlock, GetBlockHeaderObject,
        GetBlockHeaderResponse, GetBlockTransaction, GetBlockTrees, GetBlockchainInfoResponse,
        GetInfo, NetworkUpgradeInfo, NetworkUpgradeStatus, SentTransactionHash, TipConsensusBranch,
        ValidateAddresses as _,
    },
};

use crate::Height;

use super::*;

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

/// ReadStateService based validator connector.
///
/// Currently the Mempool cannot utilise the mempool change endpoint in the ReadStateService,
/// for this reason the lagacy jsonrpc inteface is used until the Mempool updates required can be implemented.
///
/// Due to the difference if the mempool inteface provided by the ReadStateService and the Json RPC service
/// two seperate Mempool implementation will likely be required.
#[derive(Clone, Debug)]
pub struct State {
    /// Used to fetch chain data.
    pub read_state_service: ReadStateService,
    /// Temporarily used to fetch mempool data.
    pub mempool_fetcher: JsonRpSeeConnector,
    /// The runtime network (activation schedule adopted from the validator).
    pub network: zebra_chain::parameters::Network,
    /// Watches the Zebra syncer's chain-tip changes; served to consumers via
    /// [`ValidatorConnector::chain_tip_change`].
    pub chain_tip_change: zebra_state::ChainTipChange,
    /// Handle to the Zebra `ReadStateService` sync task, kept alive for the lifetime of
    /// the connector and aborted on [`ValidatorConnector::shutdown`]. Shared across clones.
    pub sync_task_handle: Option<Arc<tokio::task::JoinHandle<()>>>,
}

/// A connection to a validator.
#[derive(Clone, Debug)]
// TODO: Explore whether State should be Boxed.
#[allow(clippy::large_enum_variant)]
pub enum ValidatorConnector {
    /// The connection is via direct read access to a zebrad's data file
    ///
    /// NOTE: See docs for State struct.
    State(State),
    /// We are connected to a zebrad, zcashd, or other zainod via JsonRpc ("JsonRpSee")
    Fetch(JsonRpSeeConnector),
}

/// Builds the regtest activation heights from the validator's reported
/// upgrade schedule (`getblockchaininfo.upgrades`).
///
/// The validator's configured activation heights are authoritative: the
/// config type is a payload-free kind, so both connection arms construct the
/// runtime network at first contact, before anything consumes a
/// `Network` (zaino#1076). An upgrade absent from the validator's map is
/// never-activated — nothing is backfilled from defaults. Mainnet and
/// Testnet use zebra's compiled parameters and never take this path.
fn activation_heights_from_upgrades(
    upgrades: &indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    >,
) -> Result<zaino_common::config::network::ActivationHeights, String> {
    use zebra_chain::parameters::NetworkUpgrade;

    let mut heights = zaino_common::config::network::ActivationHeights {
        before_overwinter: None,
        overwinter: None,
        sapling: None,
        blossom: None,
        heartwood: None,
        canopy: None,
        nu5: None,
        nu6: None,
        nu6_1: None,
        nu6_2: None,
        nu6_3: None,
        nu7: None,
    };
    for upgrade_info in upgrades.values() {
        let (upgrade, height, _status) = upgrade_info.into_parts();
        let slot = match upgrade {
            // Genesis is height 0 by definition; it has no configuration slot.
            NetworkUpgrade::Genesis => continue,
            NetworkUpgrade::BeforeOverwinter => &mut heights.before_overwinter,
            NetworkUpgrade::Overwinter => &mut heights.overwinter,
            NetworkUpgrade::Sapling => &mut heights.sapling,
            NetworkUpgrade::Blossom => &mut heights.blossom,
            NetworkUpgrade::Heartwood => &mut heights.heartwood,
            NetworkUpgrade::Canopy => &mut heights.canopy,
            NetworkUpgrade::Nu5 => &mut heights.nu5,
            NetworkUpgrade::Nu6 => &mut heights.nu6,
            NetworkUpgrade::Nu6_1 => &mut heights.nu6_1,
            NetworkUpgrade::Nu6_2 => &mut heights.nu6_2,
            NetworkUpgrade::Nu6_3 => &mut heights.nu6_3,
            NetworkUpgrade::Nu7 => &mut heights.nu7,
        };
        if slot.replace(height.0).is_some() {
            return Err(format!("validator reported {upgrade:?} twice"));
        }
    }
    Ok(heights)
}

impl ValidatorConnector {
    /// The JSON-RPC connector for this validator, used by the node-passthrough RPCs that
    /// have no local-index equivalent. The `State` variant proxies these through its
    /// `mempool_fetcher` until the `ReadStateService` can serve them.
    fn json_rpc_connector(&self) -> &JsonRpSeeConnector {
        match self {
            ValidatorConnector::State(state) => &state.mempool_fetcher,
            ValidatorConnector::Fetch(fetch) => fetch,
        }
    }

    /// Builds the runtime network for the configured network kind — the
    /// validator is the single source of truth for activation heights
    /// (zaino#1076): the config carries only a kind, and the runtime network
    /// is constructed here at first contact, before anything consumes a
    /// `Network` — from zebra's compiled parameters for the public networks,
    /// from the validator's reported schedule for regtest. There is no
    /// fallback: a silently wrong schedule is the failure mode this removes.
    async fn adopt_network(
        common: &CommonBackendConfig,
        fetcher: &JsonRpSeeConnector,
    ) -> Result<zebra_chain::parameters::Network, BlockchainSourceError> {
        Ok(match common.network {
            zaino_common::Network::Mainnet => zebra_chain::parameters::Network::Mainnet,
            zaino_common::Network::Testnet => {
                zebra_chain::parameters::Network::new_default_testnet()
            }
            zaino_common::Network::Regtest => {
                let blockchain_info = fetcher.get_blockchain_info().await.map_err(|error| {
                    BlockchainSourceError::Unrecoverable(format!(
                        "cannot fetch activation heights from the validator at {}: {error}",
                        common.validator_rpc_address
                    ))
                })?;
                let heights = activation_heights_from_upgrades(&blockchain_info.upgrades).map_err(
                    |reason| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "cannot adopt activation heights from the validator at {}: {reason}",
                            common.validator_rpc_address
                        ))
                    },
                )?;
                info!(?heights, "Adopted activation heights from the validator");
                heights.to_regtest_network()
            }
        })
    }

    /// Spawns a JSON-RPC-backed [`ValidatorConnector::Fetch`] from the common backend
    /// config, returning the connector plus the validator's `getinfo` response (used by
    /// the backend to build its `ServiceMetadata`).
    ///
    /// Owns the `JsonRpSeeConnector` setup that previously lived in `FetchService::spawn`.
    pub(crate) async fn spawn_fetch(
        common: &CommonBackendConfig,
    ) -> Result<(Self, GetInfoResponse, zebra_chain::parameters::Network), BlockchainSourceError>
    {
        let fetcher = JsonRpSeeConnector::new_from_config_parts(
            &common.validator_rpc_address,
            common.validator_rpc_user.clone(),
            common.validator_rpc_password.clone(),
            common.validator_cookie_path.clone(),
        )
        .await
        .map_err(BlockchainSourceError::unrecoverable)?;

        let network = Self::adopt_network(common, &fetcher).await?;

        let info = fetcher
            .get_info()
            .await
            .map_err(BlockchainSourceError::unrecoverable)?;

        Ok((ValidatorConnector::Fetch(fetcher), info, network))
    }

    /// Spawns a `ReadStateService`-backed [`ValidatorConnector::State`] from the common
    /// backend config plus the [`DirectConnectionConfig`], returning the connector plus
    /// the validator's `getinfo` response.
    ///
    /// Owns the JSON-RPC + Zebra chain-syncer setup for the `Direct` connection: builds
    /// the mempool JSON-RPC fetcher, launches the syncer and `ReadStateService`, then
    /// blocks until the syncer has caught up to the validator's best chain tip (comparing
    /// tip *hash* as well as height so a reorg mid-sync cannot report a false match). The
    /// syncer task handle is retained on the `State` so [`ValidatorConnector::shutdown`]
    /// can abort it.
    pub(crate) async fn spawn_state(
        common: &CommonBackendConfig,
        direct: &DirectConnectionConfig,
    ) -> Result<(Self, GetInfoResponse, zebra_chain::parameters::Network), BlockchainSourceError>
    {
        let map_err =
            |error: &dyn std::fmt::Display| BlockchainSourceError::Unrecoverable(error.to_string());

        let rpc_client = JsonRpSeeConnector::new_from_config_parts(
            &common.validator_rpc_address,
            common.validator_rpc_user.clone(),
            common.validator_rpc_password.clone(),
            common.validator_cookie_path.clone(),
        )
        .await
        .map_err(|error| map_err(&error))?;

        // Adopt the runtime network before the read-state syncer launches:
        // it is the first consumer of the activation schedule.
        let network = Self::adopt_network(common, &rpc_client).await?;

        let info = rpc_client
            .get_info()
            .await
            .map_err(|error| map_err(&error))?;

        info!(
            grpc_address = %direct.validator_grpc_address,
            "Launching Chain Syncer"
        );
        let (mut read_state_service, _latest_chain_tip, chain_tip_change, sync_task_handle) =
            init_read_state_with_syncer(
                direct.validator_state_config.clone(),
                &network,
                direct.validator_grpc_address,
            )
            .await
            .map_err(|error| map_err(&error))?
            .map_err(|error| map_err(&error))?;

        info!("Chain syncer launched");

        // Wait for ReadStateService to catch up to the validator's best chain tip.
        // Height alone is insufficient during reorgs: the same height can refer to
        // different blocks until JSON-RPC and ReadStateService agree on tip hash.
        loop {
            let blockchain_info = rpc_client
                .get_blockchain_info()
                .await
                .map_err(|error| map_err(&error))?;
            let server_height = blockchain_info.blocks;
            let server_tip_hash = blockchain_info.best_block_hash;

            let syncer_response = read_state_service
                .ready()
                .and_then(|service| service.call(ReadRequest::Tip))
                .await
                .map_err(BlockchainSourceError::unrecoverable)?;
            let (syncer_height, syncer_tip_hash) = expected_read_response!(syncer_response, Tip)
                .ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable("no blocks in chain".to_string())
                })?;

            if server_height == syncer_height && server_tip_hash == syncer_tip_hash {
                info!(
                    height = syncer_height.0,
                    tip_hash = %syncer_tip_hash,
                    "ReadStateService synced with Zebra"
                );
                break;
            } else {
                info!(
                    syncer_height = syncer_height.0,
                    validator_height = server_height.0,
                    syncer_tip_hash = %syncer_tip_hash,
                    validator_tip_hash = %server_tip_hash,
                    "ReadStateService syncing with Zebra"
                );
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                continue;
            }
        }

        let source = ValidatorConnector::State(State {
            read_state_service,
            mempool_fetcher: rpc_client,
            network: network.clone(),
            chain_tip_change,
            sync_task_handle: Some(Arc::new(sync_task_handle)),
        });

        Ok((source, info, network))
    }

    /// The backing [`ReadStateService`], when this connector is `State`-backed.
    ///
    /// Test-only escape hatch: live tests recompute expected chain data directly off
    /// the `ReadStateService`. Production code goes through the `ChainIndex` API.
    #[cfg(feature = "test_dependencies")]
    pub(crate) fn read_state_service(&self) -> Option<&ReadStateService> {
        match self {
            ValidatorConnector::State(state) => Some(&state.read_state_service),
            ValidatorConnector::Fetch(_) => None,
        }
    }
}

/// Serialized empty Orchard-shaped commitment tree in the RPC encoding — what a pool
/// reports when it has no treestate to serve, and the encoding both backend arms must
/// agree on.
fn empty_orchard_tree_rpc_bytes() -> Vec<u8> {
    let mut tree = vec![];
    write_commitment_tree(
        &CommitmentTree::<zebra_chain::orchard::tree::Node, 32>::empty(),
        &mut tree,
    )
    .expect("can write to Vec");
    tree
}

/// The ironwood slot of the source treestate tuple, given the ironwood treestate the
/// validator reported for the block (if any).
///
/// The z_gettreestate ironwood field is documented as "Only present from NU6.3, so that
/// pre-NU6.3 responses are unchanged": when the validator reported no ironwood treestate
/// (below NU6.3 activation, or on a network with no NU6.3 activation height) the slot
/// stays `None`, so the response omits the field exactly as zebrad does. This function
/// names that contract — do not back-fill an empty tree here.
fn ironwood_treestate_slot(validator_ironwood: Option<PoolTreestate>) -> Option<PoolTreestate> {
    validator_ironwood
}

/// Maps a validator-reported treestate (from the JSON-RPC `z_gettreestate` response)
/// into the connector's pool slot.
fn fetch_pool_treestate_slot(treestate: zebra_rpc::client::Treestate) -> Option<PoolTreestate> {
    let final_root = treestate.commitments().final_root().clone();
    treestate
        .commitments()
        .final_state()
        .clone()
        .map(|final_state| PoolTreestate {
            final_root,
            final_state,
        })
}

impl BlockchainSource for ValidatorConnector {
    /// `Some` for the `State` variant, which drives its own Zebra syncer; the
    /// JSON-RPC `Fetch` variant has no local tip-change stream and keeps the
    /// trait's `None` default semantics.
    fn chain_tip_change(&self) -> Option<zebra_state::ChainTipChange> {
        match self {
            ValidatorConnector::State(state) => Some(state.chain_tip_change.clone()),
            ValidatorConnector::Fetch(_) => None,
        }
    }

    // ********** Block methods **********

    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match self {
            ValidatorConnector::State(state) => match state
                .read_state_service
                .clone()
                .call(zebra_state::ReadRequest::Block(id))
                .await
            {
                Ok(zebra_state::ReadResponse::Block(Some(block))) => Ok(Some(block)),
                // Zebra's ReadStateService does not currently serve non-best chain blocks
                // so we must fetch using the JsonRpcConnector.
                Ok(zebra_state::ReadResponse::Block(None)) => {
                    match state.mempool_fetcher.get_block(id.to_string(), Some(0)).await
                    {
                        Ok(GetBlockResponse::Raw(raw_block)) => Ok(Some(Arc::new(
                            zebra_chain::block::Block::zcash_deserialize(raw_block.as_ref())
                                .map_err(BlockchainSourceError::unrecoverable)?,
                            ))),
                        Ok(_) => unreachable!(),
                        Err(e) => match e {
                            RpcRequestError::Method(GetBlockError::MissingBlock(_)) => Ok(None),
                            // TODO/FIX: zcashd returns this transport error when a block is requested higher than current chain. is this correct?
                            RpcRequestError::Transport(zaino_fetch::jsonrpsee::error::TransportError::ErrorStatusCode(500)) => Ok(None),
                            RpcRequestError::ServerWorkQueueFull => Err(BlockchainSourceError::Unrecoverable("Work queue full. not yet implemented: handling of ephemeral network errors.".to_string())),
                            _ => Err(BlockchainSourceError::unrecoverable(e)),
                        },
                    }
                }
                Ok(otherwise) => panic!(
                    "Read Request of Block returned Read Response of {otherwise:#?} \n\
                    This should be deterministically unreachable"
                ),
                Err(e) => Err(BlockchainSourceError::unrecoverable(e)),
            },
            ValidatorConnector::Fetch(fetch) => {
                match fetch
                    .get_block(id.to_string(), Some(0))
                    .await
                {
                    Ok(GetBlockResponse::Raw(raw_block)) => Ok(Some(Arc::new(
                        zebra_chain::block::Block::zcash_deserialize(raw_block.as_ref())
                            .map_err(BlockchainSourceError::unrecoverable)?,
                    ))),
                    Ok(_) => unreachable!(),
                    Err(e) => match e {
                        RpcRequestError::Method(GetBlockError::MissingBlock(_)) => Ok(None),
                        // TODO/FIX: zcashd returns this transport error when a block is requested higher than current chain. is this correct?
                        RpcRequestError::Transport(zaino_fetch::jsonrpsee::error::TransportError::ErrorStatusCode(500)) => Ok(None),
                        RpcRequestError::ServerWorkQueueFull => Err(BlockchainSourceError::Unrecoverable("Work queue full. not yet implemented: handling of ephemeral network errors.".to_string())),
                        _ => Err(BlockchainSourceError::unrecoverable(e)),
                    },
                }
            }
        }
    }

    async fn get_block_verbose(
        &self,
        hash_or_height: HashOrHeight,
        verbosity: Option<u8>,
    ) -> BlockchainSourceResult<GetBlock> {
        match self {
            ValidatorConnector::State(state) => {
                state.get_block_inner(hash_or_height, verbosity).await
            }
            ValidatorConnector::Fetch(fetch) => fetch
                .get_block(hash_or_height.to_string(), verbosity)
                .await
                .map_err(BlockchainSourceError::unrecoverable)?
                .try_into()
                .map_err(|error: zebra_chain::serialization::SerializationError| {
                    BlockchainSourceError::unrecoverable(error)
                }),
        }
    }

    async fn get_block_header(
        &self,
        hash: String,
        verbose: bool,
    ) -> BlockchainSourceResult<GetBlockHeader> {
        match self {
            ValidatorConnector::State(state) => {
                let hash_or_height =
                    HashOrHeight::from_str(&hash).map_err(BlockchainSourceError::unrecoverable)?;
                let header = state
                    .get_block_header_inner(hash_or_height, Some(verbose))
                    .await?;
                zebra_block_header_to_wire(header)
            }
            ValidatorConnector::Fetch(fetch) => fetch
                .get_block_header(hash, verbose)
                .await
                .map_err(BlockchainSourceError::unrecoverable),
        }
    }

    async fn get_block_deltas(&self, hash: String) -> BlockchainSourceResult<BlockDeltas> {
        match self {
            ValidatorConnector::State(state) => state.get_block_deltas(hash).await,
            ValidatorConnector::Fetch(fetch) => fetch
                .get_block_deltas(hash)
                .await
                .map_err(BlockchainSourceError::unrecoverable),
        }
    }

    // ********** Chain methods **********

    async fn get_difficulty(&self) -> BlockchainSourceResult<f64> {
        match self {
            ValidatorConnector::State(state) => chain_tip_difficulty(
                state.network.clone(),
                state.read_state_service.clone(),
                false,
            )
            .await
            .map_err(BlockchainSourceError::unrecoverable),
            ValidatorConnector::Fetch(fetch) => Ok(fetch
                .get_difficulty()
                .await
                .map_err(BlockchainSourceError::unrecoverable)?
                .0),
        }
    }

    async fn get_blockchain_info(&self) -> BlockchainSourceResult<GetBlockchainInfoResponse> {
        match self {
            ValidatorConnector::State(state) => state.get_blockchain_info().await,
            ValidatorConnector::Fetch(fetch) => fetch
                .get_blockchain_info()
                .await
                .map_err(BlockchainSourceError::unrecoverable)?
                .try_into()
                .map_err(|_error| {
                    BlockchainSourceError::Unrecoverable(
                        "getblockchaininfo: chainwork not hex-encoded integer".to_string(),
                    )
                }),
        }
    }

    // ********** Node-passthrough methods **********

    async fn get_info(&self) -> BlockchainSourceResult<GetInfo> {
        Ok(self
            .json_rpc_connector()
            .get_info()
            .await
            .map_err(BlockchainSourceError::unrecoverable)?
            .into())
    }

    async fn get_peer_info(&self) -> BlockchainSourceResult<GetPeerInfo> {
        self.json_rpc_connector()
            .get_peer_info()
            .await
            .map_err(BlockchainSourceError::unrecoverable)
    }

    async fn get_chain_tips(
        &self,
    ) -> BlockchainSourceResult<zaino_fetch::jsonrpsee::response::chain_tips::GetChainTipsResponse>
    {
        self.json_rpc_connector()
            .get_chain_tips()
            .await
            .map_err(BlockchainSourceError::unrecoverable)
    }

    async fn get_block_subsidy(&self, height: u32) -> BlockchainSourceResult<GetBlockSubsidy> {
        self.json_rpc_connector()
            .get_block_subsidy(height)
            .await
            .map_err(BlockchainSourceError::unrecoverable)
    }

    async fn get_mining_info(&self) -> BlockchainSourceResult<GetMiningInfoWire> {
        self.json_rpc_connector()
            .get_mining_info()
            .await
            .map_err(BlockchainSourceError::unrecoverable)
    }

    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> BlockchainSourceResult<GetTxOutResponse> {
        self.json_rpc_connector()
            .get_tx_out(txid, n, include_mempool)
            .await
            .map_err(BlockchainSourceError::unrecoverable)
    }

    async fn get_spent_info(
        &self,
        request: GetSpentInfoRequest,
    ) -> BlockchainSourceResult<GetSpentInfoResponse> {
        self.json_rpc_connector()
            .get_spent_info(request)
            .await
            .map_err(BlockchainSourceError::unrecoverable)
    }

    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> BlockchainSourceResult<GetNetworkSolPsResponse> {
        self.json_rpc_connector()
            .get_network_sol_ps(blocks, height)
            .await
            .map_err(BlockchainSourceError::unrecoverable)
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> BlockchainSourceResult<SentTransactionHash> {
        // ReadStateService does not yet interface with the mempool, so both variants
        // submit via the JSON-RPC connector.
        self.json_rpc_connector()
            .send_raw_transaction(raw_transaction_hex)
            .await
            .map(SentTransactionHash::from)
            .map_err(BlockchainSourceError::unrecoverable)
    }

    async fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> BlockchainSourceResult<zebra_rpc::client::GetTreestateResponse> {
        self.json_rpc_connector()
            .get_treestate(hash_or_height)
            .await
            .map_err(BlockchainSourceError::unrecoverable)?
            .try_into()
            .map_err(|_error| {
                BlockchainSourceError::Unrecoverable("failed to parse treestate".to_string())
            })
    }

    // ********** Transaction methods **********

    // Returns the transaction, and the height of the block that transaction is in if on the best chain
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    > {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher,
                ..
            }) => {
                // Check state for transaction
                let mut read_state_service = read_state_service.clone();
                let mempool_fetcher = mempool_fetcher.clone();

                let zebra_txid: zebra_chain::transaction::Hash =
                    zebra_chain::transaction::Hash::from(txid.0);

                let response = read_state_service
                    .ready()
                    .and_then(|svc| {
                        svc.call(zebra_state::ReadRequest::AnyChainTransaction(zebra_txid))
                    })
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context("state read failed", e)
                    })?;

                if let zebra_state::ReadResponse::AnyChainTransaction(opt) = response {
                    if let Some(any_chain_tx) = opt {
                        match any_chain_tx {
                            zebra_state::AnyTx::Mined(mined_tx) => {
                                return Ok(Some((
                                    (mined_tx).tx.clone(),
                                    GetTransactionLocation::BestChain(mined_tx.height),
                                )))
                            }
                            zebra_state::AnyTx::Side((transaction, _block_hash)) => {
                                return Ok(Some((
                                    transaction,
                                    GetTransactionLocation::NonbestChain,
                                )))
                            }
                        }
                    }
                } else {
                    unreachable!("unmatched response to a `Transaction` read request");
                }

                // Else check mempool for transaction.
                let mempool_txids = self.get_mempool_txids().await?.ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable(
                        "could not fetch mempool transaction ids: none returned".to_string(),
                    )
                })?;
                if mempool_txids.contains(&zebra_txid) {
                    let serialized_transaction = if let GetTransactionResponse::Raw(
                        serialized_transaction,
                    ) = mempool_fetcher
                        .get_raw_transaction(zebra_txid.to_string(), Some(0))
                        .await
                        .map_err(|e| {
                            BlockchainSourceError::unrecoverable_context(
                                "could not fetch transaction data",
                                e,
                            )
                        })? {
                        serialized_transaction
                    } else {
                        return Err(BlockchainSourceError::Unrecoverable(
                            "could not fetch transaction data: non-raw response".to_string(),
                        ));
                    };
                    let transaction: zebra_chain::transaction::Transaction =
                        zebra_chain::transaction::Transaction::zcash_deserialize(
                            std::io::Cursor::new(serialized_transaction.as_ref()),
                        )
                        .map_err(|e| {
                            BlockchainSourceError::unrecoverable_context(
                                "could not deserialize transaction data",
                                e,
                            )
                        })?;
                    Ok(Some((transaction.into(), GetTransactionLocation::Mempool)))
                } else {
                    Ok(None)
                }
            }
            ValidatorConnector::Fetch(fetch) => {
                let transaction_object = if let GetTransactionResponse::Object(transaction_object) =
                    fetch
                        .get_raw_transaction(txid.to_rpc_hex(), Some(1))
                        .await
                        .map_err(|e| {
                            BlockchainSourceError::unrecoverable_context(
                                "could not fetch transaction data",
                                e,
                            )
                        })? {
                    transaction_object
                } else {
                    return Err(BlockchainSourceError::Unrecoverable(
                        "could not fetch transaction data: non-obj response".to_string(),
                    ));
                };
                let transaction: zebra_chain::transaction::Transaction =
                    zebra_chain::transaction::Transaction::zcash_deserialize(std::io::Cursor::new(
                        transaction_object.hex().as_ref(),
                    ))
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context(
                            "could not deserialize transaction data",
                            e,
                        )
                    })?;
                let location = match transaction_object.height() {
                    Some(-1) => GetTransactionLocation::NonbestChain,
                    None => GetTransactionLocation::Mempool,
                    Some(n) => {
                        GetTransactionLocation::BestChain(n.try_into_height().map_err(|_e| {
                            BlockchainSourceError::Unrecoverable(format!(
                                "invalid height value {n}"
                            ))
                        })?)
                    }
                };
                Ok(Some((transaction.into(), location)))
            }
        }
    }

    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        let mempool_fetcher = match self {
            ValidatorConnector::State(state) => &state.mempool_fetcher,
            ValidatorConnector::Fetch(fetch) => fetch,
        };

        let txid_strings = mempool_fetcher
            .get_raw_mempool()
            .await
            .map_err(|e| {
                BlockchainSourceError::unrecoverable_context("could not fetch mempool data", e)
            })?
            .transactions;

        let txids: Vec<zebra_chain::transaction::Hash> = txid_strings
            .into_iter()
            .map(|txid_str| {
                zebra_chain::transaction::Hash::from_str(&txid_str).map_err(|e| {
                    BlockchainSourceError::unrecoverable_context(
                        format!("invalid transaction id '{txid_str}'"),
                        e,
                    )
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(Some(txids))
    }

    // ********** Chain methods **********

    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher,
                ..
            }) => {
                match read_state_service.best_tip() {
                    Some((_height, hash)) => Ok(Some(hash)),
                    None => {
                        // try RPC if state read fails:
                        Ok(Some(
                            mempool_fetcher
                                .get_best_blockhash()
                                .await
                                .map_err(|e| {
                                    BlockchainSourceError::unrecoverable_context(
                                        "could not fetch best block hash from validator",
                                        e,
                                    )
                                })?
                                .0,
                        ))
                    }
                }
            }
            ValidatorConnector::Fetch(fetch) => Ok(Some(
                fetch
                    .get_best_blockhash()
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context(
                            "could not fetch best block hash from validator",
                            e,
                        )
                    })?
                    .0,
            )),
        }
    }

    /// Returns the height of the block at the tip of the best chain.
    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>> {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher,
                ..
            }) => {
                match read_state_service.best_tip() {
                    Some((height, _hash)) => Ok(Some(height)),
                    None => {
                        // try RPC if state read fails:
                        Ok(Some(
                            mempool_fetcher
                                .get_block_count()
                                .await
                                .map_err(|e| {
                                    BlockchainSourceError::unrecoverable_context(
                                        "could not fetch best block hash from validator",
                                        e,
                                    )
                                })?
                                .into(),
                        ))
                    }
                }
            }
            ValidatorConnector::Fetch(fetch) => Ok(Some(
                fetch
                    .get_block_count()
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context(
                            "could not fetch best block hash from validator",
                            e,
                        )
                    })?
                    .into(),
            )),
        }
    }

    /// Returns the Sapling, Orchard and Ironwood treestate by blockhash.
    async fn get_treestate(
        &self,
        // Sould this be HashOrHeight?
        id: BlockHash,
    ) -> BlockchainSourceResult<super::TreestateBytes> {
        let hash_or_height: HashOrHeight = HashOrHeight::Hash(zebra_chain::block::Hash(id.into()));
        match self {
            ValidatorConnector::State(state) => {
                let mut state = state.clone();
                let block_header_response = state
                    .read_state_service
                    .ready()
                    .and_then(|service| service.call(ReadRequest::BlockHeader(hash_or_height)))
                    .await
                    .map_err(|_e| {
                        BlockchainSourceError::Unrecoverable(
                            InvalidData(format!("could not fetch header of block {id}"))
                                .to_string(),
                        )
                    })?;
                let (_header, _hash, height) = match block_header_response {
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

                let sapling = match ShieldedPool::Sapling
                    .activation_upgrade()
                    .activation_height(&state.network)
                {
                    Some(activation_height) if height >= activation_height => Some(
                        state
                            .read_state_service
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::SaplingTree(hash_or_height))
                            })
                            .await
                            .map_err(|_e| {
                                BlockchainSourceError::Unrecoverable(
                                    InvalidData(format!(
                                        "could not fetch sapling treestate of block {id}"
                                    ))
                                    .to_string(),
                                )
                            })?,
                    ),
                    _ => None,
                }
                .and_then(|sap_response| {
                    expected_read_response!(sap_response, SaplingTree).map(|tree| PoolTreestate {
                        // finalRoot exactly as zebrad serves it: display-order root bytes.
                        final_root: Some(tree.root().bytes_in_display_order().to_vec()),
                        final_state: tree.to_rpc_bytes(),
                    })
                });

                let orchard = match ShieldedPool::Orchard
                    .activation_upgrade()
                    .activation_height(&state.network)
                {
                    Some(activation_height) if height >= activation_height => Some(
                        state
                            .read_state_service
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::OrchardTree(hash_or_height))
                            })
                            .await
                            .map_err(|_e| {
                                BlockchainSourceError::Unrecoverable(
                                    InvalidData(format!(
                                        "could not fetch orchard treestate of block {id}"
                                    ))
                                    .to_string(),
                                )
                            })?,
                    ),
                    _ => None,
                }
                .and_then(|orch_response| {
                    expected_read_response!(orch_response, OrchardTree).map(|tree| PoolTreestate {
                        // finalRoot exactly as zebrad serves it: display-order root bytes.
                        final_root: Some(tree.root().bytes_in_display_order().to_vec()),
                        final_state: tree.to_rpc_bytes(),
                    })
                });

                let ironwood = match ShieldedPool::Ironwood
                    .activation_upgrade()
                    .activation_height(&state.network)
                {
                    Some(activation_height) if height >= activation_height => Some(
                        state
                            .read_state_service
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::IronwoodTree(hash_or_height))
                            })
                            .await
                            .map_err(|_e| {
                                BlockchainSourceError::Unrecoverable(
                                    InvalidData(format!(
                                        "could not fetch ironwood treestate of block {id}"
                                    ))
                                    .to_string(),
                                )
                            })?,
                    ),
                    _ => None,
                }
                .and_then(|irw_response| {
                    expected_read_response!(irw_response, IronwoodTree).map(|tree| PoolTreestate {
                        // finalRoot exactly as zebrad serves it: display-order root bytes.
                        final_root: Some(tree.root().bytes_in_display_order().to_vec()),
                        final_state: tree.to_rpc_bytes(),
                    })
                });
                let ironwood = ironwood_treestate_slot(ironwood);

                Ok((sapling, orchard, ironwood))
            }
            ValidatorConnector::Fetch(fetch) => {
                let treestate = fetch
                    .get_treestate(hash_or_height.to_string())
                    .await
                    .map_err(|_e| {
                        BlockchainSourceError::Unrecoverable(
                            InvalidData(format!("could not fetch treestate of block {id}"))
                                .to_string(),
                        )
                    })?;

                let sapling = treestate.sapling.map_or_else(
                    || {
                        let mut tree = vec![];
                        write_commitment_tree(&sapling_crypto::CommitmentTree::empty(), &mut tree)
                            .expect("can write to Vec");
                        Some(PoolTreestate {
                            final_root: None,
                            final_state: tree,
                        })
                    },
                    fetch_pool_treestate_slot,
                );

                let orchard = treestate.orchard.map_or_else(
                    || {
                        Some(PoolTreestate {
                            final_root: None,
                            final_state: empty_orchard_tree_rpc_bytes(),
                        })
                    },
                    fetch_pool_treestate_slot,
                );

                let ironwood =
                    ironwood_treestate_slot(treestate.ironwood.and_then(fetch_pool_treestate_slot));

                Ok((sapling, orchard, ironwood))
            }
        }
    }

    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>> {
        match self {
            ValidatorConnector::State(state) => {
                let start_index = NoteCommitmentSubtreeIndex(start_index);
                let limit = max_entries.map(NoteCommitmentSubtreeIndex);
                let request = match pool {
                    ShieldedPool::Sapling => ReadRequest::SaplingSubtrees { start_index, limit },
                    ShieldedPool::Orchard => ReadRequest::OrchardSubtrees { start_index, limit },
                    ShieldedPool::Ironwood => ReadRequest::IronwoodSubtrees { start_index, limit },
                };
                state
                    .read_state_service
                    .clone()
                    .call(request)
                    .await
                    .map(|response| match pool {
                        ShieldedPool::Sapling => expected_read_response!(response, SaplingSubtrees)
                            .iter()
                            .map(|(_index, subtree)| {
                                (subtree.root.to_bytes(), subtree.end_height.0)
                            })
                            .collect(),
                        ShieldedPool::Orchard => expected_read_response!(response, OrchardSubtrees)
                            .iter()
                            .map(|(_index, subtree)| (subtree.root.to_repr(), subtree.end_height.0))
                            .collect(),
                        ShieldedPool::Ironwood => {
                            expected_read_response!(response, IronwoodSubtrees)
                                .iter()
                                .map(|(_index, subtree)| {
                                    (subtree.root.to_repr(), subtree.end_height.0)
                                })
                                .collect()
                        }
                    })
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context(
                            "could not get subtrees from validator",
                            e,
                        )
                    })
            }

            ValidatorConnector::Fetch(json_rp_see_connector) => {
                let subtrees = json_rp_see_connector
                    .get_subtrees_by_index(pool.pool_string(), start_index, max_entries)
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context(
                            "could not get subtrees from validator",
                            e,
                        )
                    })?;

                Ok(subtrees
                    .subtrees
                    .iter()
                    .map(|subtree| {
                        Ok::<_, Box<dyn Error + Send + Sync>>((
                            <[u8; 32]>::try_from(hex::decode(&subtree.root)?).map_err(
                                |_subtree| {
                                    std::io::Error::new(
                                        std::io::ErrorKind::InvalidInput,
                                        "received subtree root not 32 bytes",
                                    )
                                },
                            )?,
                            subtree.end_height.0,
                        ))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context(
                            "could not get subtrees from validator",
                            e,
                        )
                    })?)
            }
        }
    }

    async fn get_commitment_tree_roots(
        &self,
        // Sould this be HashOrHeight?
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        match self {
            ValidatorConnector::State(state) => {
                let (sapling_tree_response, orchard_tree_response, ironwood_tree_response) =
                    join3(
                        state.read_state_service.clone().call(
                            zebra_state::ReadRequest::SaplingTree(HashOrHeight::Hash(id.into())),
                        ),
                        state.read_state_service.clone().call(
                            zebra_state::ReadRequest::OrchardTree(HashOrHeight::Hash(id.into())),
                        ),
                        state.read_state_service.clone().call(
                            zebra_state::ReadRequest::IronwoodTree(HashOrHeight::Hash(id.into())),
                        ),
                    )
                    .await;
                let (sapling_tree, orchard_tree, ironwood_tree) = match (
                    //TODO: Better readstateservice error handling
                    sapling_tree_response.map_err(BlockchainSourceError::unrecoverable)?,
                    orchard_tree_response.map_err(BlockchainSourceError::unrecoverable)?,
                    ironwood_tree_response.map_err(BlockchainSourceError::unrecoverable)?,
                ) {
                    (
                        ReadResponse::SaplingTree(saptree),
                        ReadResponse::OrchardTree(orctree),
                        ReadResponse::IronwoodTree(irwtree),
                    ) => (saptree, orctree, irwtree),
                    (_, _, _) => panic!("Bad response"),
                };

                Ok((
                    sapling_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                    orchard_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                    ironwood_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                ))
            }
            ValidatorConnector::Fetch(fetch) => {
                let tree_responses = fetch
                    .get_treestate(id.to_rpc_hex())
                    .await
                    // As MethodError contains a GetTreestateError, which is an enum with no variants,
                    // we don't need to account for it at all here
                    .map_err(|e| match e {
                        RpcRequestError::ServerWorkQueueFull => {
                            BlockchainSourceError::Unrecoverable(
                                "Not yet implemented: handle backing validator\
                                full queue"
                                    .to_string(),
                            )
                        }
                        _ => BlockchainSourceError::unrecoverable(e),
                    })?;
                let GetTreestateResponse {
                    sapling,
                    orchard,
                    ironwood,
                    ..
                } = tree_responses;
                let sapling_frontier = sapling
                    .map_or_else(
                        || Some(Ok(CommitmentTree::empty())),
                        |t| {
                            t.commitments().final_state().as_ref().map(|final_state| {
                                read_commitment_tree::<sapling_crypto::Node, _, 32>(
                                    final_state.as_slice(),
                                )
                            })
                        },
                    )
                    .transpose()
                    .map_err(|e| BlockchainSourceError::unrecoverable_context("io error", e))?;
                let orchard_frontier = orchard
                    .map_or_else(
                        || Some(Ok(CommitmentTree::empty())),
                        |t| {
                            t.commitments().final_state().as_ref().map(|final_state| {
                                read_commitment_tree::<zebra_chain::orchard::tree::Node, _, 32>(
                                    final_state.as_slice(),
                                )
                            })
                        },
                    )
                    .transpose()
                    .map_err(|e| BlockchainSourceError::unrecoverable_context("io error", e))?;
                let ironwood_frontier = ironwood
                    .map_or_else(
                        || Some(Ok(CommitmentTree::empty())),
                        |t| {
                            t.commitments().final_state().as_ref().map(|final_state| {
                                read_commitment_tree::<zebra_chain::orchard::tree::Node, _, 32>(
                                    final_state.as_slice(),
                                )
                            })
                        },
                    )
                    .transpose()
                    .map_err(|e| BlockchainSourceError::unrecoverable_context("io error", e))?;
                let sapling_root = sapling_frontier
                    .map(|tree| {
                        zebra_chain::sapling::tree::Root::try_from(tree.root().to_bytes())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context("could not deser", e)
                    })?;
                let orchard_root = orchard_frontier
                    .map(|tree| {
                        zebra_chain::orchard::tree::Root::try_from(tree.root().to_repr())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context("could not deser", e)
                    })?;
                let ironwood_root = ironwood_frontier
                    .map(|tree| {
                        zebra_chain::orchard::tree::Root::try_from(tree.root().to_repr())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::unrecoverable_context("could not deser", e)
                    })?;
                Ok((sapling_root, orchard_root, ironwood_root))
            }
        }
    }

    // ********** Transparent address methods **********

    async fn get_address_deltas(
        &self,
        params: GetAddressDeltasParams,
    ) -> BlockchainSourceResult<GetAddressDeltasResponse> {
        match self {
            ValidatorConnector::State(state) => {
                let mut read_state = state.read_state_service.clone();

                let (addresses, start_raw, end_raw, chain_info) = match &params {
                    GetAddressDeltasParams::Filtered {
                        addresses,
                        start,
                        end,
                        chain_info,
                    } => (addresses.clone(), *start, *end, *chain_info),
                    GetAddressDeltasParams::Address(a) => (vec![a.clone()], 0, 0, false),
                };

                let (start, end) = clamp_deltas_range_to_tip(
                    self.get_best_block_height().await?,
                    start_raw,
                    end_raw,
                )?;

                let transactions: Vec<Box<zebra_rpc::client::TransactionObject>> = {
                    let tx_ids_request =
                        GetAddressTxIdsRequest::new(addresses.clone(), Some(start.0), Some(end.0));

                    let txids = self.get_address_txids(tx_ids_request).await?;

                    let results = futures::future::join_all(
                        txids
                            .into_iter()
                            .map(|txid| async move { self.get_transaction(txid).await }),
                    )
                    .await;

                    results
                        .into_iter()
                        .map(|result| {
                            result.map(|maybe_transaction| {
                                maybe_transaction.map(|(transaction, location)| {
                                    let height = match location {
                                        GetTransactionLocation::BestChain(height) => Some(height),
                                        GetTransactionLocation::NonbestChain
                                        | GetTransactionLocation::Mempool => None,
                                    };

                                    Box::new(
                                        zebra_rpc::client::TransactionObject::from_transaction(
                                            transaction.clone(),
                                            height,
                                            None,
                                            &state.network,
                                            None,
                                            None,
                                            Some(matches!(
                                                location,
                                                GetTransactionLocation::BestChain(_)
                                            )),
                                            transaction.hash(),
                                        ),
                                    )
                                })
                            })
                        })
                        .collect::<Result<Vec<_>, BlockchainSourceError>>()?
                        .into_iter()
                        .flatten()
                        .collect()
                };

                // Ordered deltas
                let deltas = GetAddressDeltasResponse::process_transactions_to_deltas(
                    &transactions,
                    &addresses,
                );

                if chain_info && start > Height(0) && end > Height(0) {
                    let start_info = {
                        let hash_or_height =
                            HashOrHeight::Height(zebra_chain::block::Height(start.0));

                        let response = read_state
                            .ready()
                            .await
                            .map_err(BlockchainSourceError::unrecoverable)?
                            .call(ReadRequest::BlockHeader(hash_or_height))
                            .await
                            .map_err(BlockchainSourceError::unrecoverable)?;

                        match response {
                            ReadResponse::BlockHeader { hash, .. } => Ok(BlockInfo::new(
                                hex::encode(hash.bytes_in_display_order()),
                                start.0,
                            )),
                            _ => Err(BlockchainSourceError::Unrecoverable(format!(
                                "Block not found at height {}",
                                start.0
                            ))),
                        }
                    }?;

                    let end_info = {
                        let hash_or_height =
                            HashOrHeight::Height(zebra_chain::block::Height(end.0));

                        let response = read_state
                            .ready()
                            .await
                            .map_err(BlockchainSourceError::unrecoverable)?
                            .call(ReadRequest::BlockHeader(hash_or_height))
                            .await
                            .map_err(BlockchainSourceError::unrecoverable)?;

                        match response {
                            ReadResponse::BlockHeader { hash, .. } => Ok(BlockInfo::new(
                                hex::encode(hash.bytes_in_display_order()),
                                end.0,
                            )),
                            _ => Err(BlockchainSourceError::Unrecoverable(format!(
                                "Block not found at height {}",
                                end.0
                            ))),
                        }
                    }?;

                    Ok(GetAddressDeltasResponse::WithChainInfo {
                        deltas,
                        start: start_info,
                        end: end_info,
                    })
                } else {
                    // Otherwise return the array form
                    Ok(GetAddressDeltasResponse::Simple(deltas))
                }
            }
            ValidatorConnector::Fetch(fetch) => fetch
                .get_address_deltas(params)
                .await
                .map_err(BlockchainSourceError::unrecoverable),
        }
    }

    async fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<AddressBalance> {
        match self {
            ValidatorConnector::State(state) => {
                let mut state = state.read_state_service.clone();

                let strings_set = address_strings.valid_addresses().map_err(|error| {
                    BlockchainSourceError::unrecoverable_context("invalid address", error)
                })?;

                let response = state
                    .ready()
                    .and_then(|service| service.call(ReadRequest::AddressBalance(strings_set)))
                    .await
                    .map_err(BlockchainSourceError::unrecoverable)?;

                let (balance, received) = match response {
                    ReadResponse::AddressBalance { balance, received } => (balance, received),
                    unexpected => {
                        unreachable!("Unexpected response from state service: {unexpected:?}")
                    }
                };

                Ok(AddressBalance::new(balance.into(), received))
            }
            ValidatorConnector::Fetch(fetch) => Ok(fetch
                .get_address_balance(
                    address_strings
                        .valid_addresses()
                        .map_err(|_error| {
                            BlockchainSourceError::Unrecoverable(
                                "Invalid address provided".to_string(),
                            )
                        })?
                        .into_iter()
                        .map(|address| address.to_string())
                        .collect(),
                )
                .await
                .map_err(BlockchainSourceError::unrecoverable)?
                .into()),
        }
    }

    async fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> BlockchainSourceResult<Vec<TransactionHash>> {
        match self {
            ValidatorConnector::State(state) => {
                let mut state = state.read_state_service.clone();

                let (addresses, start, end) = request.into_parts();
                let response = state
                    .ready()
                    .and_then(|service| service.call(ReadRequest::Tip))
                    .await
                    .map_err(BlockchainSourceError::unrecoverable)?;

                let (chain_height, _chain_hash) = expected_read_response!(response, Tip)
                    .ok_or_else(|| {
                        BlockchainSourceError::Unrecoverable("no blocks in chain".to_string())
                    })?;

                let mut error_string = None;
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
                if let Some(error_string) = error_string {
                    return Err(BlockchainSourceError::Unrecoverable(error_string));
                }

                let request = ReadRequest::TransactionIdsByAddresses {
                    addresses: GetAddressBalanceRequest::new(addresses)
                        .valid_addresses()
                        .map_err(|error| {
                            BlockchainSourceError::unrecoverable_context("invalid address", error)
                        })?,

                    height_range: zebra_chain::block::Height(start)
                        ..=zebra_chain::block::Height(end),
                };
                let response = state
                    .ready()
                    .and_then(|service| service.call(request))
                    .await
                    .map_err(BlockchainSourceError::unrecoverable)?;

                let hashes = expected_read_response!(response, AddressesTransactionIds);

                let mut last_tx_location =
                    zebra_state::TransactionLocation::from_usize(zebra_chain::block::Height(0), 0);

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

                        TransactionHash::from(*tx_id)
                    })
                    .collect())
            }
            ValidatorConnector::Fetch(fetch) => {
                let (addresses, start, end) = request.into_parts();
                fetch
                    .get_address_txids(addresses, start, end)
                    .await
                    .map_err(BlockchainSourceError::unrecoverable)?
                    .transactions
                    .iter()
                    .map(|txid_string| {
                        TransactionHash::from_hex(txid_string.as_bytes()).map_err(|error| {
                            BlockchainSourceError::unrecoverable_context(
                                format!("invalid txid from getaddresstxids `{txid_string}`"),
                                error,
                            )
                        })
                    })
                    .collect::<Result<Vec<TransactionHash>, BlockchainSourceError>>()
            }
        }
    }

    async fn get_address_utxos(
        &self,
        addresses: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<Vec<GetAddressUtxos>> {
        match self {
            ValidatorConnector::State(state) => {
                let mut state = state.read_state_service.clone();

                let valid_addresses = addresses.valid_addresses().map_err(|error| {
                    BlockchainSourceError::unrecoverable_context("invalid address", error)
                })?;

                let request = ReadRequest::UtxosByAddresses(valid_addresses);
                let response = state
                    .ready()
                    .and_then(|service| service.call(request))
                    .await
                    .map_err(BlockchainSourceError::unrecoverable)?;

                let utxos = expected_read_response!(response, AddressUtxos);
                let mut last_output_location =
                    zebra_state::OutputLocation::from_usize(zebra_chain::block::Height(0), 0, 0);

                Ok(utxos
                    .utxos()
                    .map(
                        |(
                            utxo_address,
                            utxo_hash,
                            utxo_output_location,
                            utxo_transparent_output,
                        )| {
                            assert!(utxo_output_location > &last_output_location);
                            last_output_location = *utxo_output_location;
                            GetAddressUtxos::new(
                                utxo_address,
                                *utxo_hash,
                                utxo_output_location.output_index(),
                                utxo_transparent_output.lock_script.clone(),
                                u64::from(utxo_transparent_output.value()),
                                utxo_output_location.height(),
                            )
                        },
                    )
                    .collect())
            }
            ValidatorConnector::Fetch(fetch) => Ok(fetch
                .get_address_utxos(
                    addresses
                        .valid_addresses()
                        .map_err(|_error| {
                            BlockchainSourceError::Unrecoverable(
                                "Invalid address provided".to_string(),
                            )
                        })?
                        .into_iter()
                        .map(|address| address.to_string())
                        .collect(),
                )
                .await
                .map_err(BlockchainSourceError::unrecoverable)?
                .into_iter()
                .map(|utxos| utxos.into())
                .collect()),
        }
    }

    // ********** Utility methods **********

    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        match self {
            ValidatorConnector::State(State {
                read_state_service, ..
            }) => {
                match read_state_service
                    .clone()
                    // Empty `known_chain_tips` requests every block currently in the
                    // non-finalized state (the prior unit-variant behaviour).
                    .call(zebra_state::ReadRequest::NonFinalizedBlocksListener {
                        known_chain_tips: Default::default(),
                    })
                    .await
                {
                    Ok(ReadResponse::NonFinalizedBlocksListener(listener)) => {
                        // NOTE:  This is not Option::unwrap, but a custom zebra-defined NonFinalizedBlocksListener::unwrap.
                        Ok(Some(listener.unwrap()))
                    }
                    Ok(_) => unreachable!(),
                    Err(e) => Err(e),
                }
            }
            ValidatorConnector::Fetch(_fetch) => Ok(None),
        }
    }

    fn shutdown(&self) {
        // Only the `State` arm owns a long-lived resource: the Zebra chain-syncer
        // task feeding the `ReadStateService`. Abort it so the backend drops cleanly.
        // The `Fetch` arm holds only a stateless JSON-RPC client — nothing to tear down.
        if let ValidatorConnector::State(State {
            sync_task_handle: Some(handle),
            ..
        }) = self
        {
            handle.abort();
        }
    }
}

impl State {
    /// Builds the `getblockheader`-shaped block header from the `ReadStateService`.
    ///
    /// Returns the zebra `GetBlockHeaderResponse` shape. This is the shared State-path
    /// builder used by both [`ValidatorConnector::get_block_header`] (which converts it
    /// to the JSON-RPC wire form) and [`State::get_block_inner`] (which reuses the
    /// object fields to assemble a verbose block).
    ///
    /// Moved from the former `StateServiceSubscriber::get_block_header_inner`; the error
    /// type is now [`BlockchainSourceError`] rather than the backend error, so the
    /// zcashd-compatible `LegacyCode` on the not-in-best-chain case survives only in the
    /// message string.
    pub(super) async fn get_block_header_inner(
        &self,
        hash_or_height: HashOrHeight,
        verbose: Option<bool>,
    ) -> BlockchainSourceResult<GetBlockHeaderResponse> {
        let mut state = self.read_state_service.clone();
        let verbose = verbose.unwrap_or(true);
        let network = self.network.clone();

        let ReadResponse::BlockHeader {
            header,
            hash,
            height,
            next_block_hash,
        } = state
            .ready()
            .and_then(|service| service.call(ReadRequest::BlockHeader(hash_or_height)))
            .await
            .map_err(|_| {
                BlockchainSourceError::Unrecoverable("block height not in best chain".to_string())
            })?
        else {
            return Err(BlockchainSourceError::Unrecoverable(
                "Unexpected response to BlockHeader request".to_string(),
            ));
        };

        let response = if !verbose {
            GetBlockHeaderResponse::Raw(HexData(
                header
                    .zcash_serialize_to_vec()
                    .map_err(BlockchainSourceError::unrecoverable)?,
            ))
        } else {
            let ReadResponse::SaplingTree(sapling_tree) = state
                .ready()
                .and_then(|service| service.call(ReadRequest::SaplingTree(hash_or_height)))
                .await
                .map_err(BlockchainSourceError::unrecoverable)?
            else {
                return Err(BlockchainSourceError::Unrecoverable(
                    "Unexpected response to SaplingTree request".to_string(),
                ));
            };
            // This could be `None` if there's a chain reorg between state queries.
            let sapling_tree = sapling_tree.ok_or_else(|| {
                BlockchainSourceError::Unrecoverable("missing sapling tree for block".to_string())
            })?;

            let ReadResponse::Depth(depth) = state
                .ready()
                .and_then(|service| service.call(ReadRequest::Depth(hash)))
                .await
                .map_err(BlockchainSourceError::unrecoverable)?
            else {
                return Err(BlockchainSourceError::Unrecoverable(
                    "Unexpected response to Depth request".to_string(),
                ));
            };

            // Confirmations are one more than the depth. Depth is limited by height, so
            // it will never overflow an i64.
            let confirmations = confirmations_from_depth(depth);

            let sapling_tree_size = sapling_tree.count();
            let final_sapling_root =
                final_sapling_root(sapling_tree.root().into(), height, &network);

            let block_header = build_block_header_object(
                &header,
                hash,
                height,
                confirmations,
                final_sapling_root,
                sapling_tree_size,
                next_block_hash,
                &network,
            )?;

            GetBlockHeaderResponse::Object(Box::new(block_header))
        };

        Ok(response)
    }

    /// Builds the `getblock`-shaped verbose block from the `ReadStateService`.
    ///
    /// Moved from the former `StateServiceSubscriber::get_block_inner`; the error type is
    /// now [`BlockchainSourceError`].
    pub(super) async fn get_block_inner(
        &self,
        hash_or_height: HashOrHeight,
        verbosity: Option<u8>,
    ) -> BlockchainSourceResult<GetBlock> {
        let mut state_1 = self.read_state_service.clone();
        let verbosity = verbosity.unwrap_or(1);
        match verbosity {
            0 => {
                let request = ReadRequest::Block(hash_or_height);
                let response = state_1
                    .ready()
                    .and_then(|service| service.call(request))
                    .await
                    .map_err(BlockchainSourceError::unrecoverable)?;
                let block = expected_read_response!(response, Block);
                block
                    .map(SerializedBlock::from)
                    .map(GetBlock::Raw)
                    .ok_or_else(|| {
                        BlockchainSourceError::Unrecoverable("block not found".to_string())
                    })
            }
            1 | 2 => {
                let network = self.network.clone();
                let state_2 = self.read_state_service.clone();
                let state_4 = self.read_state_service.clone();
                let state_5 = self.read_state_service.clone();

                let blockandsize_future = {
                    let req = ReadRequest::BlockAndSize(hash_or_height);
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
                let ironwood_future = {
                    let req = ReadRequest::IronwoodTree(hash_or_height);
                    async move {
                        state_5
                            .clone()
                            .ready()
                            .and_then(|service| service.call(req))
                            .await
                    }
                };
                let block_info_future = {
                    let req = ReadRequest::BlockInfo(hash_or_height);
                    async move {
                        state_4
                            .clone()
                            .ready()
                            .and_then(|service| service.call(req))
                            .await
                    }
                };
                let (fullblock, orchard_tree_response, ironwood_tree_response, header, block_info) = futures::join!(
                    blockandsize_future,
                    orchard_future,
                    ironwood_future,
                    self.get_block_header_inner(hash_or_height, Some(true)),
                    block_info_future
                );

                let header_obj = match header? {
                    GetBlockHeaderResponse::Raw(_hex_data) => unreachable!(
                        "`true` was passed to get_block_header, an object should be returned"
                    ),
                    GetBlockHeaderResponse::Object(get_block_header_object) => {
                        get_block_header_object
                    }
                };

                let (block, size, block_info) = match (fullblock, block_info) {
                    (
                        Ok(ReadResponse::BlockAndSize(Some((block, size)))),
                        Ok(ReadResponse::BlockInfo(Some(block_info))),
                    ) => (block, size, block_info),
                    (Ok(ReadResponse::Block(None)), Ok(ReadResponse::BlockInfo(None))) => {
                        return Err(BlockchainSourceError::Unrecoverable(
                            "block not found".to_string(),
                        ));
                    }
                    (Ok(unexpected), Ok(unexpected2)) => {
                        unreachable!("Unexpected responses from state service: {unexpected:?} {unexpected2:?}")
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        return Err(BlockchainSourceError::unrecoverable(e));
                    }
                };

                let orchard_tree_response =
                    orchard_tree_response.map_err(BlockchainSourceError::unrecoverable)?;
                let orchard_tree = expected_read_response!(orchard_tree_response, OrchardTree)
                    .ok_or_else(|| {
                        BlockchainSourceError::Unrecoverable("missing orchard tree".to_string())
                    })?;

                let ironwood_tree_response =
                    ironwood_tree_response.map_err(BlockchainSourceError::unrecoverable)?;
                let ironwood_tree = expected_read_response!(ironwood_tree_response, IronwoodTree)
                    .ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable("missing ironwood tree".to_string())
                })?;

                let final_orchard_root =
                    final_orchard_root(orchard_tree.root().into(), header_obj.height(), &network);

                let (chain_supply, value_pools) = (
                    Some(GetBlockchainInfoBalance::chain_supply(
                        *block_info.value_pools(),
                    )),
                    Some(GetBlockchainInfoBalance::value_pools(
                        *block_info.value_pools(),
                        None,
                    )),
                );

                Ok(build_verbose_block(
                    &header_obj,
                    &block,
                    verbosity,
                    size as i64,
                    final_orchard_root,
                    orchard_tree.count(),
                    ironwood_tree.count(),
                    chain_supply,
                    value_pools,
                    &network,
                ))
            }
            more_than_two => Err(BlockchainSourceError::Unrecoverable(format!(
                "invalid verbosity of {more_than_two}"
            ))),
        }
    }

    /// Builds the `getblockdeltas` response from the `ReadStateService`.
    ///
    /// Moved from the former `StateServiceSubscriber::get_block_deltas`; resolves each
    /// spent prevout via a best-chain `ReadRequest::Transaction` (finalised +
    /// non-finalised), then hands off to the shared [`assemble_block_deltas`].
    async fn get_block_deltas(&self, hash: String) -> BlockchainSourceResult<BlockDeltas> {
        let hash_or_height =
            HashOrHeight::from_str(&hash).map_err(BlockchainSourceError::unrecoverable)?;
        let GetBlock::Object(object) = self.get_block_inner(hash_or_height, Some(2)).await? else {
            return Err(BlockchainSourceError::Unrecoverable(
                "getblockdeltas: unexpected raw block".to_string(),
            ));
        };

        // Per-call cache: many inputs may reference the same prevtxid, so each previous
        // transaction is fetched at most once.
        let mut prevtx_cache: HashMap<
            zebra_chain::transaction::Hash,
            Arc<zebra_chain::transaction::Transaction>,
        > = HashMap::new();
        for tx in object.tx() {
            let GetBlockTransaction::Object(txo) = tx else {
                continue;
            };
            for input in txo.inputs() {
                let Input::NonCoinbase { txid: prevtxid, .. } = input else {
                    continue;
                };
                let prev_hash = zebra_chain::transaction::Hash::from_str(prevtxid)
                    .map_err(BlockchainSourceError::unrecoverable)?;
                if prevtx_cache.contains_key(&prev_hash) {
                    continue;
                }
                let mut state = self.read_state_service.clone();
                let response = state
                    .ready()
                    .and_then(|service| service.call(ReadRequest::Transaction(prev_hash)))
                    .await
                    .map_err(BlockchainSourceError::unrecoverable)?;
                let mined_tx = expected_read_response!(response, Transaction).ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable(format!(
                        "getblockdeltas: prevout tx {prevtxid} not in best chain"
                    ))
                })?;
                prevtx_cache.insert(prev_hash, mined_tx.tx);
            }
        }

        let median_time = self.median_time_past(&object).await?;
        let network = self.network.clone();
        assemble_block_deltas(&object, &prevtx_cache, median_time, &network)
    }

    /// Median time past over the 11-block window ending at `start`, walking backwards via
    /// verbosity-1 `getblock` lookups against the `ReadStateService`.
    // TODO(DRY): MockchainSource duplicates this walk; the only difference is the
    // per-block fetch. A shared helper generic over an async block fetcher would unify them.
    async fn median_time_past(&self, start: &BlockObject) -> BlockchainSourceResult<i64> {
        const MEDIAN_TIME_PAST_WINDOW: usize = 11;
        let mut times = Vec::with_capacity(MEDIAN_TIME_PAST_WINDOW);
        let start_time = start.time().ok_or_else(|| {
            BlockchainSourceError::Unrecoverable("getblockdeltas: start block missing time".into())
        })?;
        times.push(start_time);

        let mut prev = start.previous_block_hash();
        for _ in 0..(MEDIAN_TIME_PAST_WINDOW - 1) {
            let Some(hash) = prev else {
                break; // genesis
            };
            match self
                .get_block_inner(HashOrHeight::Hash(hash), Some(1))
                .await
            {
                Ok(GetBlock::Object(object)) => {
                    if let Some(time) = object.time() {
                        times.push(time);
                    }
                    prev = object.previous_block_hash();
                }
                Ok(GetBlock::Raw(_)) => break,
                Err(_) => break, // use values collected so far
            }
        }

        median_of_block_times(times)
    }

    /// Builds the `getblockchaininfo` response from the `ReadStateService`.
    ///
    /// Moved from the former `StateServiceSubscriber::get_blockchain_info`; error type is
    /// now [`BlockchainSourceError`], and the difficulty is propagated rather than
    /// unwrapped.
    async fn get_blockchain_info(&self) -> BlockchainSourceResult<GetBlockchainInfoResponse> {
        let mut state = self.read_state_service.clone();
        let network = self.network.clone();

        let response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::TipPoolValues))
            .await
            .map_err(BlockchainSourceError::unrecoverable)?;
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
            .await
            .map_err(BlockchainSourceError::unrecoverable)?;
        let size_on_disk = expected_read_response!(usage_response, UsageInfo);

        let response = state
            .ready()
            .and_then(|service| service.call(ReadRequest::BlockHeader(hash.into())))
            .await
            .map_err(BlockchainSourceError::unrecoverable)?;
        let header = match response {
            ReadResponse::BlockHeader { header, .. } => header,
            unexpected => {
                unreachable!("Unexpected response from state service: {unexpected:?}")
            }
        };

        let now = Utc::now();
        let zebra_estimated_height =
            NetworkChainTipHeightEstimator::new(header.time, height, &network)
                .estimate_height_at(now);
        let estimated_height = if header.time > now || zebra_estimated_height < height {
            height
        } else {
            zebra_estimated_height
        };

        let upgrades = IndexMap::from_iter(network.full_activation_list().into_iter().filter_map(
            |(activation_height, network_upgrade)| {
                // Zebra defines network upgrades by consensus rule changes, zcashd by ZIPs;
                // upgrades with a consensus branch ID are the same in both.
                network_upgrade.branch_id().map(|branch_id| {
                    // zcashd's RPC ignores Disabled network upgrades, so Zebra does too.
                    let status = if height >= activation_height {
                        NetworkUpgradeStatus::Active
                    } else {
                        NetworkUpgradeStatus::Pending
                    };
                    (
                        ConsensusBranchIdHex::new(branch_id.into()),
                        NetworkUpgradeInfo::from_parts(network_upgrade, activation_height, status),
                    )
                })
            },
        ));

        let next_block_height =
            (height + 1).expect("valid chain tips are a lot less than Height::MAX");
        let consensus = TipConsensusBranch::from_parts(
            ConsensusBranchIdHex::new(
                NetworkUpgrade::current(&network, height)
                    .branch_id()
                    .unwrap_or(ConsensusBranchId::RPC_MISSING_ID)
                    .into(),
            )
            .inner(),
            ConsensusBranchIdHex::new(
                NetworkUpgrade::current(&network, next_block_height)
                    .branch_id()
                    .unwrap_or(ConsensusBranchId::RPC_MISSING_ID)
                    .into(),
            )
            .inner(),
        );

        let difficulty =
            chain_tip_difficulty(network.clone(), self.read_state_service.clone(), false)
                .await
                .map_err(BlockchainSourceError::unrecoverable)?;

        let verification_progress = f64::from(height.0) / f64::from(zebra_estimated_height.0);

        Ok(GetBlockchainInfoResponse::new(
            network.bip70_network_name(),
            height,
            hash,
            estimated_height,
            GetBlockchainInfoBalance::chain_supply(balance),
            // TODO: account for new delta_pools arg?
            GetBlockchainInfoBalance::value_pools(balance, None),
            upgrades,
            consensus,
            height,
            difficulty,
            verification_progress,
            // TODO: store work in the finalized state for each height
            // (see https://github.com/ZcashFoundation/zebra/issues/7109)
            0,
            false,
            size_on_disk,
            // TODO (copied from zebra): investigate whether this needs implementing
            // (it's sprout-only in zcashd)
            0,
        ))
    }
}

/// Confirmations are one more than the depth, or -1 when the block is not on the best
/// chain. Depth is limited by height, so it never overflows an `i64`.
pub(crate) fn confirmations_from_depth(depth: Option<u32>) -> i64 {
    const NOT_IN_BEST_CHAIN_CONFIRMATIONS: i64 = -1;
    depth
        .map(|depth| i64::from(depth) + 1)
        .unwrap_or(NOT_IN_BEST_CHAIN_CONFIRMATIONS)
}

/// The `finalsaplingroot` field: the sapling note-commitment tree root in display
/// (big-endian) byte order once Sapling has activated, else all-zero.
pub(crate) fn final_sapling_root(
    root: [u8; 32],
    height: zebra_chain::block::Height,
    network: &zebra_chain::parameters::Network,
) -> [u8; 32] {
    match NetworkUpgrade::Sapling.activation_height(network) {
        Some(activation) if height >= activation => {
            let mut root = root;
            root.reverse();
            root
        }
        _ => [0; 32],
    }
}

/// The `finalorchardroot` field: `Some(root)` once NU5 has activated, else `None`.
pub(crate) fn final_orchard_root(
    root: [u8; 32],
    height: zebra_chain::block::Height,
    network: &zebra_chain::parameters::Network,
) -> Option<[u8; 32]> {
    match NetworkUpgrade::Nu5.activation_height(network) {
        Some(activation) if height >= activation => Some(root),
        _ => None,
    }
}

/// Assembles a `getblockheader` object from its already-resolved primitive pieces.
///
/// Shared by the [`ValidatorConnector`] State path (which reads the pieces from the
/// `ReadStateService`) and by `MockchainSource` (which reads them from test vectors),
/// so the two never drift.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_block_header_object(
    header: &Header,
    hash: zebra_chain::block::Hash,
    height: zebra_chain::block::Height,
    confirmations: i64,
    final_sapling_root: [u8; 32],
    sapling_tree_size: u64,
    next_block_hash: Option<zebra_chain::block::Hash>,
    network: &zebra_chain::parameters::Network,
) -> BlockchainSourceResult<GetBlockHeaderObject> {
    let mut nonce = *header.nonce;
    nonce.reverse();
    let difficulty = header.difficulty_threshold.relative_to_network(network);
    let block_commitments =
        header_to_block_commitments(header, network, height, final_sapling_root)?;

    Ok(GetBlockHeaderObject::new(
        hash,
        confirmations,
        height,
        header.version,
        header.merkle_root,
        block_commitments,
        final_sapling_root,
        sapling_tree_size,
        header.time.timestamp(),
        nonce,
        header.solution,
        header.difficulty_threshold,
        difficulty,
        header.previous_block_hash,
        next_block_hash,
    ))
}

/// Assembles a verbose (`verbosity` 1 or 2) `getblock` object from a resolved header
/// object plus the block's transactions and pool aggregates.
///
/// Shared by the [`ValidatorConnector`] State path and `MockchainSource`. `chain_supply`
/// / `value_pools` are the cumulative pool balances (present for the validator, `None`
/// for the mock, whose vectors don't carry them).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_verbose_block(
    header_obj: &GetBlockHeaderObject,
    block: &zebra_chain::block::Block,
    verbosity: u8,
    size: i64,
    final_orchard_root: Option<[u8; 32]>,
    orchard_tree_size: u64,
    ironwood_tree_size: u64,
    chain_supply: Option<GetBlockchainInfoBalance>,
    value_pools: Option<[GetBlockchainInfoBalance; 6]>,
    network: &zebra_chain::parameters::Network,
) -> GetBlock {
    let transactions_response: Vec<GetBlockTransaction> = block
        .transactions
        .iter()
        .map(|transaction| match verbosity {
            1 => GetBlockTransaction::Hash(transaction.hash()),
            2 => GetBlockTransaction::Object(Box::new(TransactionObject::from_transaction(
                transaction.clone(),
                Some(header_obj.height()),
                Some(header_obj.confirmations()),
                network,
                DateTime::<Utc>::from_timestamp(header_obj.time(), 0),
                Some(header_obj.hash()),
                // A non-optional header height indicates a mainchain block; sidechain
                // data is out of scope for now. TODO: return Some(true/false) once resolved.
                None,
                transaction.hash(),
            ))),
            _ => unreachable!("verbosity known to be 1 or 2"),
        })
        .collect();

    let trees = GetBlockTrees::new(
        header_obj.sapling_tree_size(),
        orchard_tree_size,
        ironwood_tree_size,
    );
    let transaction_count = transactions_response.len();

    GetBlock::Object(Box::new(BlockObject::new(
        header_obj.hash(),
        header_obj.confirmations(),
        Some(size),
        Some(header_obj.height()),
        Some(header_obj.version()),
        Some(header_obj.merkle_root()),
        Some(header_obj.block_commitments()),
        Some(header_obj.final_sapling_root()),
        final_orchard_root,
        transaction_count,
        transactions_response,
        Some(header_obj.time()),
        Some(header_obj.nonce()),
        Some(header_obj.solution()),
        Some(header_obj.bits()),
        Some(header_obj.difficulty()),
        chain_supply,
        value_pools,
        trees,
        Some(header_obj.previous_block_hash()),
        header_obj.next_block_hash(),
    )))
}

/// Computes the `blockcommitments` field for a block header.
///
/// Moved verbatim (bar the error type) from the former `state.rs`
/// `header_to_block_commitments`.
pub(crate) fn header_to_block_commitments(
    header: &Header,
    network: &zebra_chain::parameters::Network,
    height: zebra_chain::block::Height,
    final_sapling_root: [u8; 32],
) -> BlockchainSourceResult<[u8; 32]> {
    let hash = match header.commitment(network, height).map_err(|error| {
        BlockchainSourceError::unrecoverable_context("invalid block commitment", error)
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

/// Converts the zebra `GetBlockHeaderResponse` (Raw/Object) into the JSON-RPC wire
/// `getblockheader` response (Compact/Verbose) expected by the backends.
///
/// The two shapes serialize to the same JSON, so a serde round-trip is the least-error-
/// prone conversion.
pub(crate) fn zebra_block_header_to_wire(
    header: GetBlockHeaderResponse,
) -> BlockchainSourceResult<GetBlockHeader> {
    let value = serde_json::to_value(&header).map_err(BlockchainSourceError::unrecoverable)?;
    serde_json::from_value(value).map_err(BlockchainSourceError::unrecoverable)
}

/// Returns the median of a non-empty set of block times.
pub(crate) fn median_of_block_times(mut times: Vec<i64>) -> BlockchainSourceResult<i64> {
    if times.is_empty() {
        return Err(BlockchainSourceError::Unrecoverable(
            "getblockdeltas: no block times collected for median".to_string(),
        ));
    }
    times.sort_unstable();
    Ok(times[times.len() / 2])
}

/// Maps a verbose-transaction output to its `getblockdeltas` output delta,
/// or `None` when the output does not participate in address deltas: only
/// outputs with exactly one derivable address are attributed. Zero means no
/// address to credit (nonstandard script); more than one (e.g. bare multisig)
/// has no single owning address, and zcashd's getblockdeltas omits such
/// outputs rather than crediting the first address.
fn output_delta_from_verbose(vout: &zebra_rpc::client::Output) -> Option<OutputDelta> {
    let address =
        vout.script_pub_key()
            .addresses()
            .as_ref()
            .and_then(|addresses| match addresses.as_slice() {
                [address] => Some(address.clone()),
                _ => None,
            })?;
    let satoshis: Amount<NonNegative> = vout.value_zat().try_into().ok()?;
    Some(OutputDelta {
        address,
        satoshis,
        index: vout.n(),
    })
}

/// Assembles a `getblockdeltas` response from a verbosity-2 `getblock` object plus a
/// cache of previous transactions (keyed by txid) needed to resolve each spend's address
/// and value, its median time, and the running network.
///
/// Shared by the [`ValidatorConnector`] State path and `MockchainSource`: both resolve
/// the prevout transactions their own way, then hand the assembled inputs here so the
/// delta-shaping logic lives in one place.
pub(crate) fn assemble_block_deltas(
    object: &BlockObject,
    prevtx_cache: &HashMap<
        zebra_chain::transaction::Hash,
        Arc<zebra_chain::transaction::Transaction>,
    >,
    median_time: i64,
    network: &zebra_chain::parameters::Network,
) -> BlockchainSourceResult<BlockDeltas> {
    let mut deltas = Vec::with_capacity(object.tx().len());
    for (tx_index, tx) in object.tx().iter().enumerate() {
        let GetBlockTransaction::Object(txo) = tx else {
            return Err(BlockchainSourceError::Unrecoverable(
                "getblockdeltas: unexpected hash when expecting object".to_string(),
            ));
        };
        let txid = txo.txid().to_string();

        let mut inputs: Vec<InputDelta> = Vec::new();
        for (i, vin) in txo.inputs().iter().enumerate() {
            let (prevtxid, prevout) = match vin {
                Input::Coinbase { .. } => continue,
                Input::NonCoinbase {
                    txid: prevtxid,
                    vout: prevout,
                    ..
                } => (prevtxid, *prevout),
            };

            let prev_hash = zebra_chain::transaction::Hash::from_str(prevtxid)
                .map_err(BlockchainSourceError::unrecoverable)?;
            let prev_tx = prevtx_cache.get(&prev_hash).ok_or_else(|| {
                BlockchainSourceError::Unrecoverable(format!(
                    "getblockdeltas: prevout tx {prevtxid} not resolved"
                ))
            })?;

            let output = prev_tx.outputs().get(prevout as usize).ok_or_else(|| {
                BlockchainSourceError::Unrecoverable(format!(
                    "getblockdeltas: prevout index {prevout} out of range for {prevtxid}"
                ))
            })?;

            // Nonstandard script ⇒ no derivable address ⇒ skip (matches the outputs branch).
            let address = match output.address(network) {
                Some(address) => address.to_string(),
                None => continue,
            };

            // Inputs are debits, so the amount leaves the address.
            let satoshis: Amount = (-output.value().zatoshis()).try_into().map_err(|error| {
                BlockchainSourceError::unrecoverable_context(
                    format!("getblockdeltas: input amount out of range for {prevtxid}:{prevout}"),
                    error,
                )
            })?;

            inputs.push(InputDelta {
                address,
                satoshis,
                index: i as u32,
                prevtxid: prevtxid.clone(),
                prevout,
            });
        }

        let outputs: Vec<OutputDelta> = txo
            .outputs()
            .iter()
            .filter_map(output_delta_from_verbose)
            .collect();

        deltas.push(BlockDelta {
            txid,
            index: tx_index as u32,
            inputs,
            outputs,
        });
    }

    Ok(BlockDeltas {
        hash: object.hash().to_string(),
        confirmations: object.confirmations(),
        size: object.size().ok_or_else(|| {
            BlockchainSourceError::Unrecoverable("getblockdeltas: block size missing".to_string())
        })?,
        height: object
            .height()
            .ok_or_else(|| {
                BlockchainSourceError::Unrecoverable(
                    "getblockdeltas: block height missing".to_string(),
                )
            })?
            .0,
        version: object.version().ok_or_else(|| {
            BlockchainSourceError::Unrecoverable(
                "getblockdeltas: block version missing".to_string(),
            )
        })?,
        merkle_root: object
            .merkle_root()
            .ok_or_else(|| {
                BlockchainSourceError::Unrecoverable(
                    "getblockdeltas: block merkle root missing".to_string(),
                )
            })?
            .encode_hex::<String>(),
        deltas,
        time: object.time().ok_or_else(|| {
            BlockchainSourceError::Unrecoverable("getblockdeltas: block time missing".to_string())
        })?,
        median_time,
        nonce: hex::encode(object.nonce().ok_or_else(|| {
            BlockchainSourceError::Unrecoverable("getblockdeltas: block nonce missing".to_string())
        })?),
        bits: object
            .bits()
            .ok_or_else(|| {
                BlockchainSourceError::Unrecoverable(
                    "getblockdeltas: block bits missing".to_string(),
                )
            })?
            .to_string(),
        difficulty: object.difficulty().ok_or_else(|| {
            BlockchainSourceError::Unrecoverable(
                "getblockdeltas: block difficulty missing".to_string(),
            )
        })?,
        previous_block_hash: object.previous_block_hash().map(|hash| hash.to_string()),
        next_block_hash: object.next_block_hash().map(|hash| hash.to_string()),
    })
}

#[cfg(test)]
mod zebra_block_header_to_wire {
    use super::*;

    /// The Direct connection's non-verbose `getblockheader` builds a
    /// `GetBlockHeaderResponse::Raw` and crosses to the wire type through the
    /// serde round-trip under test: the raw hex must land in the untagged
    /// `Compact` variant with the same bytes, or every non-verbose
    /// `getblockheader` on a Direct connection errors at runtime. The
    /// mockchain `get_block_header` test covers the same conversion end to
    /// end; this pins it at its definition.
    #[test]
    fn raw_header_round_trips_to_compact_hex() {
        let header_bytes = vec![0x01, 0x02, 0xab, 0xcd];
        let raw = GetBlockHeaderResponse::Raw(HexData(header_bytes.clone()));

        let wire = zebra_block_header_to_wire(raw)
            .expect("the raw header shape must convert to the wire type");

        assert_eq!(wire, GetBlockHeader::Compact(hex::encode(header_bytes)));
    }
}

#[cfg(test)]
mod fetch_pool_treestate_slot {
    /// Regression test: the validator's finalRoot must pass through to the pool slot.
    /// zebra populates `Commitments { finalRoot, finalState }` for every pool it
    /// serves, but the slot mapping dropped the root, so zaino's z_gettreestate
    /// responses carried no finalRoot for any pool.
    ///
    #[test]
    fn final_root_passes_through() {
        let final_root = vec![7u8; 32];
        let final_state = vec![1u8, 2, 3];
        let treestate = zebra_rpc::client::Treestate::new(zebra_rpc::client::Commitments::new(
            Some(final_root.clone()),
            Some(final_state.clone()),
        ));

        let slot = super::fetch_pool_treestate_slot(treestate)
            .expect("a treestate with a finalState maps to a populated slot");

        assert_eq!(slot.final_state, final_state);
        assert_eq!(
            slot.final_root,
            Some(final_root),
            "the validator's finalRoot must pass through to the treestate slot"
        );
    }

    /// An absent finalState maps to an absent slot.
    #[test]
    fn absent_final_state_maps_to_absent_slot() {
        let treestate =
            zebra_rpc::client::Treestate::new(zebra_rpc::client::Commitments::new(None, None));
        assert_eq!(super::fetch_pool_treestate_slot(treestate), None);
    }
}

/// Clamps a `getaddressdeltas` height range to the current best tip:
/// `end == 0` means "to the tip", and both bounds clamp down to it. A source
/// that reports no best height (nothing indexed yet) is a typed error.
fn clamp_deltas_range_to_tip(
    tip: Option<zebra_chain::block::Height>,
    start_raw: u32,
    end_raw: u32,
) -> BlockchainSourceResult<(Height, Height)> {
    let tip: Height = tip
        .ok_or_else(|| {
            BlockchainSourceError::Unrecoverable(
                "getaddressdeltas: the source reports no best block height".to_string(),
            )
        })?
        .into();
    let mut start = Height(start_raw);
    let mut end = Height(end_raw);
    if end == Height(0) || end > tip {
        end = tip;
    }
    if start > tip {
        start = tip;
    }
    Ok((start, end))
}

#[cfg(test)]
mod output_delta_from_verbose {
    use super::*;

    fn output_with_addresses(addresses: Option<Vec<String>>) -> zebra_rpc::client::Output {
        zebra_rpc::client::Output::new(
            0.00000001,
            1,
            0,
            zebra_rpc::client::ScriptPubKey::new(
                "asm".to_string(),
                zebra_chain::transparent::Script::new(&[0u8]),
                addresses.as_ref().map(|a| a.len() as u32),
                "pubkeyhash".to_string(),
                addresses,
            ),
        )
    }

    /// An output with exactly one derivable address is attributed to it.
    #[test]
    fn single_address_output_is_attributed() {
        let delta =
            output_delta_from_verbose(&output_with_addresses(Some(vec!["t1address".to_string()])))
                .expect("a single-address output participates in deltas");
        assert_eq!(delta.address, "t1address");
    }

    /// An output with multiple derivable addresses (e.g. bare multisig) has no
    /// single owning address: zcashd's getblockdeltas omits it, and crediting
    /// the first address would fabricate balance for it. The pre-fix code took
    /// `addresses.first()`.
    #[test]
    fn multi_address_output_is_omitted() {
        assert_eq!(
            output_delta_from_verbose(&output_with_addresses(Some(vec![
                "t1first".to_string(),
                "t1second".to_string(),
            ]))),
            None,
            "a multi-address output must be omitted, not attributed to the first address"
        );
    }

    /// No derivable address (nonstandard script) means no delta.
    #[test]
    fn addressless_output_is_omitted() {
        assert_eq!(
            output_delta_from_verbose(&output_with_addresses(None)),
            None
        );
    }
}

#[cfg(test)]
mod clamp_deltas_range_to_tip {
    use super::*;

    /// A source that reports no best height (nothing indexed yet, or the
    /// RPC fallback found no tip) must yield a typed error, not a panic:
    /// this range clamp previously lived inline in `get_address_deltas`
    /// behind `get_best_block_height().await?.unwrap()`.
    #[test]
    fn absent_tip_is_a_typed_error() {
        assert!(
            clamp_deltas_range_to_tip(None, 0, 0).is_err(),
            "an absent tip must surface as a BlockchainSourceError"
        );
    }

    /// `end == 0` means "to the tip", and both bounds clamp down to the tip.
    #[test]
    fn bounds_clamp_to_tip() {
        let tip = Some(zebra_chain::block::Height(100));

        let (start, end) =
            clamp_deltas_range_to_tip(tip, 5, 0).expect("a present tip clamps successfully");
        assert_eq!((start, end), (Height(5), Height(100)));

        let (start, end) =
            clamp_deltas_range_to_tip(tip, 5, 400).expect("a present tip clamps successfully");
        assert_eq!((start, end), (Height(5), Height(100)));

        let (start, end) =
            clamp_deltas_range_to_tip(tip, 300, 50).expect("a present tip clamps successfully");
        assert_eq!((start, end), (Height(100), Height(50)));
    }
}

#[cfg(test)]
mod get_block_verbose {
    use super::*;

    /// zaino-serve recovers zcashd-compatible RPC error codes by downcast-walking
    /// [`std::error::Error::source`] chains (see
    /// `getblock_error_object_from_indexer_error` and
    /// `sendrawtransaction_error_object_from_indexer_error` in
    /// `zaino-serve/src/rpc/jsonrpc/service.rs`). The connector boundary must
    /// therefore preserve the typed cause instead of flattening it to a string,
    /// or `getblock` failures surface as generic internal errors rather than the
    /// legacy `-8` code lightwalletd-family clients key on.
    #[tokio::test]
    async fn fetch_error_keeps_transport_error_in_source_chain() {
        // Port 1 refuses connections, so the request fails at the transport
        // layer without contacting any validator.
        let connector = zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector::new_with_basic_auth(
            "http://127.0.0.1:1/"
                .parse()
                .expect("static url literal parses"),
            "user".to_string(),
            "password".to_string(),
        )
        .expect("connector construction is network-free");
        let source = ValidatorConnector::Fetch(connector);

        let error = source
            .get_block_verbose(HashOrHeight::Height(zebra_chain::block::Height(1)), Some(1))
            .await
            .expect_err("a request to a closed port must fail");

        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&error);
        let mut transport_error_reachable = false;
        while let Some(source_error) = current {
            if source_error
                .downcast_ref::<zaino_fetch::jsonrpsee::error::TransportError>()
                .is_some()
            {
                transport_error_reachable = true;
                break;
            }
            current = source_error.source();
        }
        assert!(
            transport_error_reachable,
            "the typed TransportError must stay reachable via the source() chain; \
             stringifying it strips the RPC error code the serve layer recovers"
        );
    }
}

#[cfg(test)]
mod ironwood_treestate_slot {
    /// Regression test: when the validator reported no ironwood treestate (below NU6.3
    /// activation, or on a network with no NU6.3 activation height) the slot must stay
    /// `None`, so z_gettreestate omits the ironwood field exactly as zebrad does
    /// ("Only present from NU6.3, so that pre-NU6.3 responses are unchanged"). The slot
    /// was previously back-filled with a serialized empty tree, emitting the field at
    /// every height on every network.
    #[test]
    fn absent_validator_ironwood_stays_absent() {
        assert_eq!(
            super::ironwood_treestate_slot(None),
            None,
            "ironwood slot must stay absent when the validator reported no ironwood treestate"
        );
    }

    /// A reported ironwood treestate passes through unchanged.
    #[test]
    fn reported_validator_ironwood_passes_through() {
        let treestate = crate::chain_index::source::PoolTreestate {
            final_root: None,
            final_state: vec![1u8, 2, 3],
        };
        assert_eq!(
            super::ironwood_treestate_slot(Some(treestate.clone())),
            Some(treestate)
        );
    }
}

#[cfg(test)]
mod activation_heights_from_upgrades {
    use zaino_common::config::network::ActivationHeights;

    /// All-`None` heights: the starting point adoption fills from the
    /// validator's map, and the expected value for every absent upgrade.
    const NEVER_ACTIVATED: ActivationHeights = ActivationHeights {
        before_overwinter: None,
        overwinter: None,
        sapling: None,
        blossom: None,
        heartwood: None,
        canopy: None,
        nu5: None,
        nu6: None,
        nu6_1: None,
        nu6_2: None,
        nu6_3: None,
        nu7: None,
    };

    fn upgrades_map(
        json: &str,
    ) -> indexmap::IndexMap<
        zebra_rpc::methods::ConsensusBranchIdHex,
        zebra_rpc::methods::NetworkUpgradeInfo,
    > {
        serde_json::from_str(json).expect("upgrades fixture parses")
    }

    fn adopted_heights(
        upgrades: &indexmap::IndexMap<
            zebra_rpc::methods::ConsensusBranchIdHex,
            zebra_rpc::methods::NetworkUpgradeInfo,
        >,
    ) -> ActivationHeights {
        super::activation_heights_from_upgrades(upgrades).expect("valid schedule")
    }

    /// An upgrade absent from the validator's map is never-activated —
    /// nothing is backfilled from any default schedule.
    #[test]
    fn leaves_absent_upgrades_never_activated() {
        let upgrades = upgrades_map(
            r#"{
                "c2d6d0b4": { "name": "NU5", "activationheight": 2, "status": "active" },
                "c8e71055": { "name": "NU6", "activationheight": 2, "status": "active" }
            }"#,
        );

        assert_eq!(
            adopted_heights(&upgrades),
            ActivationHeights {
                nu5: Some(2),
                nu6: Some(2),
                ..NEVER_ACTIVATED
            }
        );
    }

    /// The ORCHARD_THEN_IRONWOOD transition shape: everything through NU6.2
    /// at 1–2, NU6.3 at 6 — the schedule the ironwood_activation fixtures
    /// launch validators with.
    #[test]
    fn reads_a_transition_schedule() {
        let upgrades = upgrades_map(
            r#"{
                "5ba81b19": { "name": "Overwinter", "activationheight": 1, "status": "active" },
                "76b809bb": { "name": "Sapling", "activationheight": 1, "status": "active" },
                "2bb40e60": { "name": "Blossom", "activationheight": 1, "status": "active" },
                "f5b9230b": { "name": "Heartwood", "activationheight": 1, "status": "active" },
                "e9ff75a6": { "name": "Canopy", "activationheight": 1, "status": "active" },
                "c2d6d0b4": { "name": "NU5", "activationheight": 2, "status": "active" },
                "c8e71055": { "name": "NU6", "activationheight": 2, "status": "active" },
                "4dec4df0": { "name": "NU6.1", "activationheight": 2, "status": "active" },
                "5437f330": { "name": "NU6.2", "activationheight": 2, "status": "active" },
                "37a5165b": { "name": "NU6.3", "activationheight": 6, "status": "pending" }
            }"#,
        );

        assert_eq!(
            adopted_heights(&upgrades),
            ActivationHeights {
                overwinter: Some(1),
                sapling: Some(1),
                blossom: Some(1),
                heartwood: Some(1),
                canopy: Some(1),
                nu5: Some(2),
                nu6: Some(2),
                nu6_1: Some(2),
                nu6_2: Some(2),
                nu6_3: Some(6),
                ..NEVER_ACTIVATED
            }
        );
    }

    /// A validator reporting the same upgrade twice is nonsense; adoption
    /// must fail loudly rather than pick a height.
    #[test]
    fn rejects_a_duplicate_upgrade() {
        let upgrades = upgrades_map(
            r#"{
                "c2d6d0b4": { "name": "NU5", "activationheight": 2, "status": "active" },
                "c8e71055": { "name": "NU5", "activationheight": 3, "status": "pending" }
            }"#,
        );

        let reason =
            super::activation_heights_from_upgrades(&upgrades).expect_err("duplicate must fail");
        assert!(
            reason.contains("twice"),
            "error should name the duplication, got: {reason}"
        );
    }
}
