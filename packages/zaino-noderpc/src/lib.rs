//! `zaino-noderpc` — POC Zcash node JSON-RPC serve adapter.
//!
//! The node-RPC sibling of the light-serve adapter, bound to [`NodeRpcService`]
//! alone. It reads domain types through a pinned snapshot and converts
//! **domain <-> wire in the adapter** (see [`wire`]) — both directions, because
//! node RPC is input-heavy (hex params in, hex/JSON out).
//!
//! A slice (four methods), not the production JSON-RPC server, and it stands up
//! no jsonrpsee server — it exercises the handler shape against the mock.
//! Chain/node-info aggregates and the validator passthrough (mining/peers/
//! txoutset) are not modelled here.
#![forbid(unsafe_code)]

mod error;
mod rpc;
mod transport;
pub mod wire;

pub use error::RpcError;
pub use rpc::NodeRpcApiServer;
pub use transport::{JsonRpcServeError, JsonRpcServer};

use zaino_primitives::types::{Height, TransactionLocation, TransparentAddress};
use zaino_service::error::ReadError;
use zaino_service::queries;
use zaino_service::BlockVerboseRead;
use zaino_service::RawTransactionRead;
use zaino_service::TransactionViewRead;
use zaino_service::{BlockRead, ChainInfoRead, ChainSegment, NodeRpcService};
use zcash_protocol::consensus::Network;

use zaino_primitives::types::BlockSelector;

use crate::wire::params::{AddressDeltasParam, AddressesParam};
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltasResponse, BlockHeaderResponse, BlockchainInfoResponse,
    DeltaRange, GetBlockResponse, GetRawTransactionResponse, MiningInfoResponse, NodeInfoResponse,
    PeerInfoEntry, RawTransactionResponse, UnifiedReceiversResponse, ValidateAddressResponse,
    ZValidateAddressResponse,
};
use crate::wire::{
    address_balance_to_wire, block_header_to_wire, block_to_wire_v1, block_to_wire_v2,
    blockchain_info_to_wire, blockhash_from_hex, bytes_from_hex, bytes_to_hex, delta_to_wire,
    mining_info_to_wire, node_info_to_wire, peer_info_to_wire, to_hex, transaction_view_to_wire,
    txid_from_hex, unified_receivers_to_wire, validated_to_wire, z_validated_to_wire,
};

/// Zcash node JSON-RPC handler over a [`NodeRpcService`] engine.
///
/// Carries the network because two served methods — `validateaddress` and
/// `z_validateaddress` — are pure functions of an address and a network, with
/// no chain read at all. The network is a serving parameter, not a capability,
/// so it lives on the adapter.
#[derive(Clone)]
pub struct NodeRpc<S: NodeRpcService> {
    engine: S,
    network: Network,
}

impl<S: NodeRpcService> NodeRpc<S> {
    /// Build the handler over `engine`, validating addresses against `network`.
    pub fn new(engine: S, network: Network) -> Self {
        Self { engine, network }
    }

    /// `getblockcount`: the height of the pinned tip.
    pub async fn get_block_count(&self) -> Result<u32, RpcError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(RpcError::NoBlocks)?;
        Ok(tip.height.into())
    }

    /// `getbestblockhash`: the hash of the pinned tip, as hex.
    pub async fn get_best_block_hash(&self) -> Result<String, RpcError> {
        let snapshot = self.engine.snapshot().await?;
        let tip = snapshot.pinned_tip().ok_or(RpcError::NoBlocks)?;
        Ok(to_hex(tip.hash.into()))
    }

    /// `sendrawtransaction`: decode hex, relay, return the txid. A rejection is
    /// an RPC error here (contrast the light-serve `SendResponse`).
    pub async fn send_raw_transaction(&self, tx_hex: &str) -> Result<String, RpcError> {
        let raw = bytes_from_hex(tx_hex)?;
        let txid = self.engine.broadcast(raw).await?;
        Ok(to_hex(txid.into()))
    }

    /// `getrawtransaction`: the transaction's consensus bytes as hex (verbosity
    /// 0) or the decoded explorer object (verbosity 1).
    ///
    /// Verbosity 1 resolves every transparent input to the value and address of
    /// the output it spends, and adds the chain-location fields (`height`,
    /// `confirmations`, `blockhash`, `time`, `blocktime`) — all absent for a
    /// mempool transaction, which has no containing block. A verbosity outside
    /// `0..=1` is a parameter error naming the served range.
    pub async fn get_raw_transaction(
        &self,
        txid_hex: &str,
        verbosity: Option<u32>,
    ) -> Result<GetRawTransactionResponse, RpcError> {
        let txid = txid_from_hex(txid_hex)?;
        match verbosity.unwrap_or(0) {
            0 => {
                let snapshot = self.engine.snapshot().await?;
                let found = snapshot.raw_transaction(txid).await?;
                let tx = found.ok_or_else(|| {
                    RpcError::NotFound(format!("no transaction with id {txid_hex}"))
                })?;
                Ok(GetRawTransactionResponse::Raw(bytes_to_hex(&tx.data)))
            }
            1 => {
                let snapshot = self.engine.snapshot().await?;
                let located = snapshot.transaction_view(txid).await?.ok_or_else(|| {
                    RpcError::NotFound(format!("no transaction with id {txid_hex}"))
                })?;
                let transaction = transaction_view_to_wire(&located.view, &self.network);
                // Chain-location fields: present for a mined transaction, absent
                // for one in the mempool or a side chain.
                let (height, confirmations, blockhash, time, blocktime) = match &located.location {
                    TransactionLocation::BestChain(block_height) => {
                        let height: u32 = (*block_height).into();
                        let confirmations = snapshot.pinned_tip().map(|tip| {
                            let tip_height: u32 = tip.height.into();
                            i64::from(tip_height) - i64::from(height) + 1
                        });
                        // The block carries the hash and time; a miss here is a
                        // reorg race on an otherwise-located transaction, so the
                        // three block fields are omitted rather than erroring.
                        let block = snapshot
                            .block(BlockSelector::Height(*block_height))
                            .await
                            .map_err(ReadError::from)?;
                        match block {
                            Some(block) => (
                                Some(height),
                                confirmations,
                                Some(to_hex(block.header.hash.into())),
                                Some(block.header.time),
                                Some(block.header.time),
                            ),
                            None => (Some(height), confirmations, None, None, None),
                        }
                    }
                    TransactionLocation::NonBestChain | TransactionLocation::Mempool => {
                        (None, None, None, None, None)
                    }
                };
                Ok(GetRawTransactionResponse::Verbose(Box::new(
                    RawTransactionResponse {
                        transaction,
                        height,
                        confirmations,
                        blockhash,
                        time,
                        blocktime,
                    },
                )))
            }
            other => Err(RpcError::InvalidParams(format!(
                "verbosity {other} is out of range; getrawtransaction serves 0 (raw hex) and 1 (the decoded transaction)"
            ))),
        }
    }

    /// `getblockchaininfo` (aggregate): reads the validator's `BlockchainInfo` —
    /// the node-rpc read delta the wallet-shaped ports lack — and renders it as
    /// zcashd's response. An unreachable validator surfaces as an RPC error
    /// (via `?`), never a response with defaulted fields.
    pub async fn get_blockchain_info(&self) -> Result<BlockchainInfoResponse, RpcError> {
        let snapshot = self.engine.snapshot().await?;
        let info = snapshot.chain_info().await?;
        Ok(blockchain_info_to_wire(info))
    }

    /// `getblock`: the raw block hex at verbosity 0, or the block object at
    /// verbosity 1 (transaction ids) or 2 (decoded transactions) — the block
    /// page's and search page's read.
    ///
    /// The block id arrives as a string: all-digits is a height, otherwise a hex
    /// block hash (the explorer sends a height as a decimal string). zcashd
    /// defaults to verbosity 1, and the explorer calls verbosity 1 on every block
    /// page, so an omitted verbosity is 1. Verbosity 0 returns the consensus bytes
    /// as lowercase hex — the explorer's search page calls it to test whether a
    /// string is a block; a verbosity above 2 is a parameter error naming the
    /// served range. An unknown block is a not-found error at every verbosity.
    ///
    /// At verbosity 1 and 2 the response composes three reads for the same block:
    /// its header ([`BlockRead::block`]), its chain position
    /// ([`BlockVerboseRead::block_verbose`]), and its transactions. A by-height
    /// request resolves the height to a hash once — from the header read — and
    /// issues the position and transaction reads by that hash, so the three reads
    /// cannot straddle a tip reorg between them. The third read
    /// differs by verbosity: verbosity 1 takes size and transaction ids from
    /// [`TransactionViewRead::decoded_block`], which resolves **no** prevouts, so
    /// the hot per-page call never fans out over every input or fails on a prevout
    /// the validator cannot serve; verbosity 2 takes the decoded transactions with
    /// every input resolved from [`TransactionViewRead::block_transaction_views`].
    /// All three missing is a not-found error; a subset present is a reorg race
    /// (transient), never a partially rendered block.
    pub async fn get_block(
        &self,
        blockid: &str,
        verbosity: Option<u32>,
    ) -> Result<GetBlockResponse, RpcError> {
        let requested = verbosity.unwrap_or(1);
        if requested > 2 {
            return Err(RpcError::InvalidParams(format!(
                "verbosity {requested} is out of range; getblock serves 0 (raw hex), 1 (transaction ids) and 2 (decoded transactions)"
            )));
        }
        let selector = block_selector_from_str(blockid)?;
        let snapshot = self.engine.snapshot().await?;
        if requested == 0 {
            // Verbosity 0: the raw consensus bytes as lowercase hex. The explorer's
            // search page calls this to test whether a string is a block; an unknown
            // block is the same not-found as the verbose arms report.
            let raw = snapshot
                .raw_block(selector)
                .await
                .map_err(ReadError::from)?
                .ok_or_else(|| RpcError::NotFound(format!("no block for {blockid}")))?;
            return Ok(GetBlockResponse::Raw(bytes_to_hex(&raw)));
        }
        let block = snapshot.block(selector).await.map_err(ReadError::from)?;
        // Resolve the height to a hash once, from this first read, so the chain
        // position and transaction reads below cannot straddle a tip reorg between
        // them: a by-hash read always names the one block the header came from.
        // When the block read misses there is nothing to render, so the original
        // selector still drives the subset/not-found detection.
        let contents = match &block {
            Some(block) => BlockSelector::Hash(block.header.hash),
            None => selector,
        };
        let verbose = snapshot
            .block_verbose(contents)
            .await
            .map_err(ReadError::from)?;
        // A subset of the reads present is a reorg race between them — transient,
        // never a partial render; all absent is a genuine miss.
        let disagree = || {
            RpcError::Read(ReadError::Transient(format!(
                "block {blockid} and its contents disagree; retry"
            )))
        };
        let not_found = || RpcError::NotFound(format!("no block for {blockid}"));
        if requested == 1 {
            // Verbosity 1 (the hot per-page call) uses the decoded block, which
            // resolves no prevouts: size and transaction ids only.
            let decoded = snapshot.decoded_block(contents).await?;
            match (block, verbose, decoded) {
                (Some(block), Some(verbose), Some(decoded)) => Ok(GetBlockResponse::Verbose1(
                    block_to_wire_v1(&block, &verbose, &decoded),
                )),
                (None, None, None) => Err(not_found()),
                _ => Err(disagree()),
            }
        } else {
            // Verbosity 2 resolves every transparent input to the output it spends.
            let views = snapshot.block_transaction_views(contents).await?;
            match (block, verbose, views) {
                (Some(block), Some(verbose), Some(views)) => Ok(GetBlockResponse::Verbose2(
                    block_to_wire_v2(&block, &verbose, &views, &self.network),
                )),
                (None, None, None) => Err(not_found()),
                _ => Err(disagree()),
            }
        }
    }

    /// `getblockheader`: the verbose block header for a hash — zcashd's default
    /// `verbose = true` shape. `[hash]` only; the explorer's blocks-by-date list
    /// fans this out per hash. A hash no retained chain holds is a not-found RPC
    /// error, never a defaulted header.
    pub async fn get_block_header(&self, hash_hex: &str) -> Result<BlockHeaderResponse, RpcError> {
        let hash = blockhash_from_hex(hash_hex)?;
        let snapshot = self.engine.snapshot().await?;
        let header = snapshot
            .block_header_verbose(hash)
            .await
            .map_err(ReadError::from)?
            .ok_or_else(|| RpcError::NotFound(format!("no block with hash {hash_hex}")))?;
        Ok(block_header_to_wire(header))
    }

    /// `getinfo`: the validator's self-description, relayed. Not indexed. An
    /// unreachable or not-ready validator is an RPC error (via `?`), never a
    /// response with defaulted fields.
    pub async fn get_info(&self) -> Result<NodeInfoResponse, RpcError> {
        Ok(node_info_to_wire(self.engine.node_info().await?))
    }

    /// `getmininginfo`: the validator's mining view, relayed. Not indexed.
    pub async fn get_mining_info(&self) -> Result<MiningInfoResponse, RpcError> {
        Ok(mining_info_to_wire(self.engine.mining_info().await?))
    }

    /// `getpeerinfo`: the validator's connected peers, relayed. Not indexed. An
    /// empty list is a valid answer from an isolated validator.
    pub async fn get_peer_info(&self) -> Result<Vec<PeerInfoEntry>, RpcError> {
        Ok(self
            .engine
            .peer_info()
            .await?
            .into_iter()
            .map(peer_info_to_wire)
            .collect())
    }

    /// `getnetworksolps`: the network solution rate, relayed. `blocks` and
    /// `height` are forwarded as given, so `None` means the validator's own
    /// defaults rather than a value this adapter invents.
    pub async fn get_network_sol_ps(
        &self,
        blocks: Option<u32>,
        height: Option<u32>,
    ) -> Result<u64, RpcError> {
        let height = height
            .map(Height::try_from)
            .transpose()
            .map_err(|_| RpcError::InvalidParams("height is not a valid height".into()))?;
        Ok(self.engine.network_sol_ps(blocks, height).await?)
    }

    /// `getaddressbalance`: the transparent balance of the requested addresses,
    /// summed. zcashd accepts a list and returns one total, so a multi-address
    /// request sums rather than returning a per-address breakdown.
    pub async fn get_address_balance(
        &self,
        params: AddressesParam,
    ) -> Result<AddressBalanceResponse, RpcError> {
        if params.addresses.is_empty() {
            return Err(RpcError::InvalidParams(
                "addresses must not be empty".into(),
            ));
        }
        let snapshot = self.engine.snapshot().await?;
        let addrs: Vec<TransparentAddress> = params
            .addresses
            .into_iter()
            .map(TransparentAddress::new)
            .collect();
        // Explorer policy (an unserviceable snapshot reads as zero) lives in the
        // shared query layer, not here; this handler only validates its wire
        // parameters and renders the domain answer.
        let total = queries::address_balance(&snapshot, &addrs).await?;
        Ok(address_balance_to_wire(total))
    }

    /// `getaddressdeltas`: every balance change touching the requested
    /// addresses.
    ///
    /// The domain answer — which range was queried, in what order, and what no
    /// coverage means — comes from [`queries::address_deltas`]. This renders it,
    /// and applies `chainInfo`, which is a wire choice about whether the range
    /// is echoed back.
    pub async fn get_address_deltas(
        &self,
        params: AddressDeltasParam,
    ) -> Result<AddressDeltasResponse, RpcError> {
        if params.addresses.is_empty() {
            return Err(RpcError::InvalidParams(
                "addresses must not be empty".into(),
            ));
        }
        let start = params
            .start
            .map(Height::try_from)
            .transpose()
            .map_err(|_| RpcError::InvalidParams("start is not a valid height".into()))?;
        let end = params
            .end
            .map(Height::try_from)
            .transpose()
            .map_err(|_| RpcError::InvalidParams("end is not a valid height".into()))?;
        let addrs: Vec<TransparentAddress> = params
            .addresses
            .into_iter()
            .map(TransparentAddress::new)
            .collect();

        let snapshot = self.engine.snapshot().await?;
        let answer = queries::address_deltas(&snapshot, &addrs, start, end).await?;
        Ok(AddressDeltasResponse {
            deltas: answer.deltas.into_iter().map(delta_to_wire).collect(),
            range: params
                .chain_info
                .then_some(answer.range)
                .flatten()
                .map(|range| DeltaRange {
                    start: range.start.into(),
                    end: range.end.into(),
                }),
        })
    }

    /// `validateaddress`: classify a transparent address against the serving
    /// network. No chain read — a pure function of the string and the network.
    pub async fn validate_address(
        &self,
        address: &str,
    ) -> Result<ValidateAddressResponse, RpcError> {
        Ok(validated_to_wire(zaino_address::validate_address(
            address.to_owned(),
            &self.network,
        )))
    }

    /// `z_validateaddress`: the deprecated shielded-aware classification.
    pub async fn z_validate_address(
        &self,
        address: &str,
    ) -> Result<ZValidateAddressResponse, RpcError> {
        Ok(z_validated_to_wire(zaino_address::z_validate_address(
            address.to_owned(),
            &self.network,
        )))
    }

    /// `z_listunifiedreceivers`: the receivers a unified address bundles, each
    /// re-encoded standalone. A pure function of the address and the network.
    ///
    /// An address that is not unified is a parameter error, not an empty
    /// result: the caller asked about a specific string, and reporting "no
    /// receivers" would imply a valid unified address that bundles nothing.
    pub async fn z_list_unified_receivers(
        &self,
        address: &str,
    ) -> Result<UnifiedReceiversResponse, RpcError> {
        let receivers = zaino_address::list_unified_receivers(address.to_owned(), &self.network)
            .ok_or_else(|| {
                RpcError::InvalidParams(format!("{address} is not a unified address"))
            })?;
        Ok(unified_receivers_to_wire(receivers))
    }
}

/// Parse `getblock`'s block id (wire -> domain input validation): an all-digits
/// string is a height, anything else a hex block hash. The explorer sends a
/// height as a decimal string and a hash as hex.
fn block_selector_from_str(blockid: &str) -> Result<BlockSelector, RpcError> {
    if !blockid.is_empty() && blockid.bytes().all(|b| b.is_ascii_digit()) {
        let height = blockid
            .parse::<u32>()
            .ok()
            .and_then(|h| Height::try_from(h).ok())
            .ok_or_else(|| RpcError::InvalidParams(format!("{blockid} is not a valid height")))?;
        Ok(BlockSelector::Height(height))
    } else {
        Ok(BlockSelector::Hash(blockhash_from_hex(blockid)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{block_selector_from_str, NodeRpc, RpcError};
    use crate::wire::response::{GetBlockResponse, GetRawTransactionResponse};
    use zaino_primitives::types::{BlockHash, BlockRef, Height, TransactionId};
    use zaino_service::testing::{MockChain, MockIndexerService};
    use zcash_protocol::consensus::Network;

    fn engine_with_tip(tip: Option<BlockRef>) -> MockIndexerService {
        MockIndexerService::new(MockChain {
            tip,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn block_count_and_best_hash_read_the_pinned_tip() {
        let tip = BlockRef {
            height: Height::try_from(291).expect("valid height"),
            hash: BlockHash::from([0xCDu8; 32]),
        };
        let node = NodeRpc::new(engine_with_tip(Some(tip)), Network::MainNetwork);
        assert_eq!(node.get_block_count().await.expect("count"), 291);
        assert_eq!(
            node.get_best_block_hash().await.expect("hash"),
            "cd".repeat(32)
        );
    }

    #[tokio::test]
    async fn send_raw_transaction_decodes_and_relays() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        let txid = node
            .send_raw_transaction("deadbeef")
            .await
            .expect("broadcast");
        assert_eq!(txid, "0".repeat(64)); // mock returns the zero txid
        assert!(matches!(
            node.send_raw_transaction("odd").await,
            Err(RpcError::InvalidParams(_))
        ));
    }

    #[tokio::test]
    async fn peer_info_and_network_solps_read_the_node_status_port() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(node.get_peer_info().await.expect("peers").is_empty());
        assert_eq!(node.get_network_sol_ps(None, None).await.expect("solps"), 0);
    }

    /// Review Focus 1 at the adapter boundary: a node-status failure becomes an
    /// RPC error, not a default-valued success. The service mock answers
    /// `getinfo` with `NotReady`.
    #[tokio::test]
    async fn an_unavailable_node_info_is_an_rpc_error() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_info().await,
            Err(RpcError::NodeStatus(_))
        ));
    }

    /// Explorer contract: `getinfo` MUST carry a string `build`. The explorer's
    /// homepage pattern-matches `{:ok, %{"build" => build}}` and 500s without it,
    /// so this golden test pins both its presence and its type.
    #[tokio::test]
    async fn getinfo_build_is_a_present_string() {
        use zaino_primitives::types::rpc::NodeInfo;
        use zaino_primitives::types::Zatoshis;
        let info = NodeInfo {
            version: 5_008_025,
            build: "v5.8.0".to_string(),
            subversion: "/MagicBean:5.8.0/".to_string(),
            protocol_version: 170_100,
            blocks: Height::try_from(2_500_000).expect("valid height"),
            connections: 8,
            difficulty: 1_234.5,
            testnet: false,
            proxy: None,
            pay_tx_fee: Zatoshis::new(1_000).expect("valid amount"),
            relay_fee: Zatoshis::new(100).expect("valid amount"),
            errors: None,
            errors_timestamp: None,
        };
        let wire = crate::wire::node_info_to_wire(info);
        let json = serde_json::to_value(&wire).expect("serialize");
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("build").and_then(serde_json::Value::as_str),
            Some("v5.8.0"),
            "the explorer's homepage reads build as a string: {obj:?}"
        );
    }

    #[tokio::test]
    async fn chain_info_reads_render_the_scripted_aggregate() {
        use zaino_primitives::types::{
            BlockchainInfo, ConsensusBranchId, ConsensusBranchIds, ValuePoolBalance, Zatoshis,
        };
        // Chain-info aggregate: a node-rpc-specific indexed read, scripted so the
        // real response's fields are distinguishable from defaults.
        let scripted = BlockchainInfo {
            chain: "main".to_string(),
            blocks: Height::try_from(77).expect("valid height"),
            headers: Height::try_from(78).expect("valid height"),
            estimated_height: Height::try_from(79).expect("valid height"),
            best_block_hash: BlockHash::from([0x22u8; 32]),
            difficulty: 42.5,
            verification_progress: 0.5,
            chain_work: None,
            pruned: false,
            size_on_disk: 9_000,
            commitments: 3,
            chain_supply: ValuePoolBalance {
                id: String::new(),
                chain_value: Zatoshis::new(1_000).expect("valid amount"),
                monitored: true,
                value_delta: None,
            },
            value_pools: vec![ValuePoolBalance {
                id: "orchard".to_string(),
                chain_value: Zatoshis::new(500).expect("valid amount"),
                monitored: true,
                value_delta: None,
            }],
            upgrades: Vec::new(),
            consensus: ConsensusBranchIds {
                chain_tip: ConsensusBranchId::new(0),
                next_block: ConsensusBranchId::new(0),
            },
        };
        let engine = MockIndexerService::new(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(77).expect("valid height"),
                hash: BlockHash::from([0x22u8; 32]),
            }),
            blockchain_info: Some(scripted),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        let info = node.get_blockchain_info().await.expect("chain info");
        assert_eq!(info.chain, "main");
        assert_eq!(info.blocks, 77);
        assert_eq!(info.headers, 78);
        assert_eq!(info.estimated_height, 79);
        assert_eq!(info.best_block_hash, "22".repeat(32));
        assert_eq!(info.difficulty, 42.5);
        assert_eq!(info.size_on_disk, 9_000);
        assert_eq!(info.commitments, 3);
        assert_eq!(info.value_pools.len(), 1);
        assert_eq!(info.value_pools[0].id, "orchard");
        assert_eq!(info.value_pools[0].chain_value_zat, 500);
    }

    fn scripted_block_and_verbose() -> (
        zaino_primitives::types::Block,
        zaino_primitives::types::BlockVerbose,
    ) {
        use zaino_primitives::types::{
            AbsoluteChainWork, Block, BlockHeader, BlockTreeSizes, BlockVerbose, ChainMetadata,
            CompactDifficulty, EquihashSolution, OrchardData, SaplingData, Script, TransparentData,
            TransparentInput, TransparentOutput, TreeSize, Zatoshis,
        };
        let mut work_bytes = [0u8; 32];
        work_bytes[28..].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let coinbase = zaino_primitives::types::Transaction {
            txid: TransactionId::from([0xC0; 32]),
            transparent: TransparentData {
                inputs: Vec::new(),
                outputs: vec![TransparentOutput {
                    value: Zatoshis::new(625_000_000).expect("valid amount"),
                    script: Script::new(vec![]),
                }],
            },
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        };
        let spend = zaino_primitives::types::Transaction {
            txid: TransactionId::from([0x7A; 32]),
            transparent: TransparentData {
                inputs: vec![TransparentInput {
                    prev_txid: TransactionId::from([0x01; 32]),
                    prev_index: 0,
                }],
                outputs: Vec::new(),
            },
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        };
        let block = Block {
            header: BlockHeader {
                hash: BlockHash::from([0x11; 32]),
                version: 4,
                prev_hash: BlockHash::from([0x22; 32]),
                height: Height::try_from(2_468).expect("valid height"),
                time: 1_600_000_000,
                merkle_root: [0x33; 32].into(),
                block_commitments: [0x44; 32].into(),
                bits: CompactDifficulty::try_from_bits(0x1f07_ffff).expect("valid nBits"),
                nonce: [0x55; 32],
                solution: EquihashSolution::Regtest([0; 36]),
            },
            transactions: vec![coinbase, spend],
            chain_metadata: ChainMetadata::ZERO,
        };
        let verbose = BlockVerbose {
            confirmations: 9,
            difficulty: 123.5,
            chainwork: AbsoluteChainWork::try_from_reported(work_bytes).expect("in-range work"),
            chain_supply: None,
            value_pools: Vec::new(),
            tree_sizes: BlockTreeSizes {
                sapling: TreeSize::from(1u32),
                orchard: TreeSize::from(2u32),
                ironwood: TreeSize::from(3u32),
            },
            next_block_hash: None,
        };
        (block, verbose)
    }

    /// A block's transactions as views, for the `block_transaction_views` read:
    /// a coinbase (its input from the detail) and a transparent spend with its
    /// prevout resolved, plus a serialized size.
    fn scripted_views() -> zaino_service::BlockTransactionViews {
        use zaino_primitives::types::{
            CoinbaseInput, OrchardData, SaplingData, Script, TransactionDetail, TransparentData,
            TransparentInput, TransparentOutput, Zatoshis,
        };
        use zaino_service::{BlockTransactionViews, ResolvedInput, TransactionView};

        let coinbase = zaino_primitives::types::Transaction {
            txid: TransactionId::from([0xC0; 32]),
            transparent: TransparentData::default(),
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        };
        let coinbase_detail = TransactionDetail {
            version: 4,
            overwintered: true,
            version_group_id: Some(0x892f_2085),
            lock_time: 0,
            expiry_height: Some(Height::try_from(0).expect("valid height")),
            size: 100,
            coinbase: Some(CoinbaseInput {
                script: Script::new(vec![0x03, 0x01, 0x02, 0x03]),
                sequence: 0xffff_ffff,
            }),
            joinsplits: Vec::new(),
        };

        let spend = zaino_primitives::types::Transaction {
            txid: TransactionId::from([0x7A; 32]),
            transparent: TransparentData {
                inputs: vec![TransparentInput {
                    prev_txid: TransactionId::from([0x01; 32]),
                    prev_index: 0,
                }],
                outputs: Vec::new(),
            },
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        };
        let spend_detail = TransactionDetail {
            version: 4,
            overwintered: true,
            version_group_id: Some(0x892f_2085),
            lock_time: 0,
            expiry_height: Some(Height::try_from(0).expect("valid height")),
            size: 180,
            coinbase: None,
            joinsplits: Vec::new(),
        };
        let resolved = ResolvedInput {
            outpoint: TransparentInput {
                prev_txid: TransactionId::from([0x01; 32]),
                prev_index: 0,
            },
            spent: TransparentOutput {
                value: Zatoshis::new(500).expect("valid amount"),
                script: Script::new(vec![]),
            },
        };

        BlockTransactionViews {
            size: 999,
            transactions: vec![
                TransactionView {
                    transaction: coinbase,
                    detail: coinbase_detail,
                    inputs: Vec::new(),
                },
                TransactionView {
                    transaction: spend,
                    detail: spend_detail,
                    inputs: vec![resolved],
                },
            ],
        }
    }

    /// The same two transactions as [`scripted_views`], but as the *unresolved*
    /// decoded block — what `getblock` verbosity 1 reads. No prevouts, no inputs.
    fn scripted_decoded_block() -> zaino_primitives::types::DecodedBlock {
        use zaino_primitives::types::{
            CoinbaseInput, DecodedBlock, DetailedTransaction, OrchardData, SaplingData, Script,
            TransactionDetail, TransparentData, TransparentInput,
        };
        let coinbase = zaino_primitives::types::Transaction {
            txid: TransactionId::from([0xC0; 32]),
            transparent: TransparentData::default(),
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        };
        let coinbase_detail = TransactionDetail {
            version: 4,
            overwintered: true,
            version_group_id: Some(0x892f_2085),
            lock_time: 0,
            expiry_height: Some(Height::try_from(0).expect("valid height")),
            size: 100,
            coinbase: Some(CoinbaseInput {
                script: Script::new(vec![0x03, 0x01, 0x02, 0x03]),
                sequence: 0xffff_ffff,
            }),
            joinsplits: Vec::new(),
        };
        let spend = zaino_primitives::types::Transaction {
            txid: TransactionId::from([0x7A; 32]),
            transparent: TransparentData {
                inputs: vec![TransparentInput {
                    prev_txid: TransactionId::from([0x01; 32]),
                    prev_index: 0,
                }],
                outputs: Vec::new(),
            },
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        };
        let spend_detail = TransactionDetail {
            version: 4,
            overwintered: true,
            version_group_id: Some(0x892f_2085),
            lock_time: 0,
            expiry_height: Some(Height::try_from(0).expect("valid height")),
            size: 180,
            coinbase: None,
            joinsplits: Vec::new(),
        };
        DecodedBlock {
            size: 999,
            transactions: vec![
                DetailedTransaction {
                    transaction: coinbase,
                    detail: coinbase_detail,
                },
                DetailedTransaction {
                    transaction: spend,
                    detail: spend_detail,
                },
            ],
        }
    }

    #[tokio::test]
    async fn get_block_composes_the_block_its_position_and_its_transactions() {
        let (block, verbose) = scripted_block_and_verbose();
        let engine = MockIndexerService::new(MockChain {
            block: Some(block),
            block_verbose: Some(verbose),
            block_transaction_views: Some(scripted_views()),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        // A height arrives as a decimal string; verbosity 2 decodes the txs.
        match node.get_block("2468", Some(2)).await.expect("block served") {
            GetBlockResponse::Verbose2(got) => {
                assert_eq!(got.hash, "11".repeat(32));
                assert_eq!(got.height, 2_468);
                assert_eq!(got.confirmations, 9);
                assert_eq!(got.difficulty, 123.5);
                assert_eq!(got.size, 999); // from BlockTransactionViews, not the header
                assert_eq!(got.tx.len(), 2);
            }
            other => panic!("verbosity 2 must render decoded transactions: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_block_verbosity_one_lists_transaction_ids() {
        let (block, verbose) = scripted_block_and_verbose();
        let engine = MockIndexerService::new(MockChain {
            block: Some(block),
            block_verbose: Some(verbose),
            // Verbosity 1 reads the decoded block, not the resolved views.
            decoded_block: Some(scripted_decoded_block()),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        // Verbosity 1 is the hot block-page path: tx is a list of id strings.
        match node.get_block("2468", Some(1)).await.expect("block served") {
            GetBlockResponse::Verbose1(got) => {
                assert_eq!(got.size, 999);
                assert_eq!(got.tx, vec!["c0".repeat(32), "7a".repeat(32)]);
            }
            other => panic!("verbosity 1 must list transaction ids: {other:?}"),
        }
    }

    /// R50: a by-height `getblock` resolves the height to a hash once — from the
    /// block read — then reads the chain position and transactions by that hash,
    /// so the three reads cannot straddle a tip reorg between them. The mock
    /// answers the by-height and by-hash selectors with different blocks; every
    /// component of the rendered response must come from the hash-selected one,
    /// never the by-height re-read.
    #[tokio::test]
    async fn get_block_by_height_reads_contents_by_the_resolved_hash() {
        use zaino_primitives::types::DecodedBlock;
        let (block, mut by_height_verbose) = scripted_block_and_verbose();
        // The block read (always by height here) fixes the hash the rest resolve
        // by; its header hash is `0x11…`.
        by_height_verbose.confirmations = 111;
        let mut by_hash_verbose = by_height_verbose.clone();
        by_hash_verbose.confirmations = 222;
        // Distinct decoded blocks: the by-height one would render size 500 and no
        // transaction ids, the by-hash one size 999 and the two scripted ids.
        let by_height_decoded = DecodedBlock {
            size: 500,
            transactions: Vec::new(),
        };
        let engine = MockIndexerService::new(MockChain {
            block: Some(block),
            block_verbose: Some(by_height_verbose),
            block_verbose_by_hash: Some(by_hash_verbose),
            decoded_block: Some(by_height_decoded),
            decoded_block_by_hash: Some(scripted_decoded_block()),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        match node.get_block("2468", Some(1)).await.expect("block served") {
            GetBlockResponse::Verbose1(got) => {
                assert_eq!(
                    got.confirmations, 222,
                    "the chain position is read by the resolved hash, not by height again"
                );
                assert_eq!(
                    got.size, 999,
                    "the size is read by the resolved hash, not by height again"
                );
                assert_eq!(
                    got.tx,
                    vec!["c0".repeat(32), "7a".repeat(32)],
                    "the transaction ids are read by the resolved hash"
                );
            }
            other => panic!("verbosity 1 must render the hash-selected block: {other:?}"),
        }
    }

    /// R46: verbosity 1 must not resolve prevouts. A block whose prevout
    /// resolution would fail (`block_transaction_views` errors with
    /// `MissingPrevout`) still renders at verbosity 1, which reads the unresolved
    /// decoded block — while verbosity 2, which does resolve, surfaces the error.
    #[tokio::test]
    async fn get_block_verbosity_one_survives_a_prevout_resolution_failure() {
        use zaino_primitives::types::TransparentInput;
        let (block, verbose) = scripted_block_and_verbose();
        let engine = MockIndexerService::new(MockChain {
            block: Some(block),
            block_verbose: Some(verbose),
            decoded_block: Some(scripted_decoded_block()),
            // Resolution would fail: a prevout the validator cannot serve.
            block_transaction_views_missing_prevout: Some(TransparentInput {
                prev_txid: TransactionId::from([0x01; 32]),
                prev_index: 0,
            }),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);

        // Verbosity 1 renders — it never touched the failing resolution.
        match node.get_block("2468", Some(1)).await.expect("v1 served") {
            GetBlockResponse::Verbose1(got) => assert_eq!(got.tx.len(), 2),
            other => panic!("verbosity 1 must render ids without resolving: {other:?}"),
        }
        // Verbosity 2 resolves, so the source inconsistency surfaces.
        assert!(matches!(
            node.get_block("2468", Some(2)).await,
            Err(RpcError::TransactionView(_))
        ));
    }

    #[tokio::test]
    async fn get_block_verbosity_zero_returns_the_raw_hex() {
        // The explorer's search page calls `getblock(query, 0)` to test whether a
        // string is a block; a hit is the consensus bytes as lowercase hex.
        let engine = MockIndexerService::new(MockChain {
            raw_block: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        match node.get_block("2468", Some(0)).await.expect("block served") {
            GetBlockResponse::Raw(hex) => assert_eq!(hex, "deadbeef"),
            other => panic!("verbosity 0 must render raw hex: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_block_verbosity_zero_reports_an_unknown_block_as_not_found() {
        // Nothing scripted: the raw-block read misses, the same not-found the
        // verbose arms report.
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_block("999999", Some(0)).await,
            Err(RpcError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_block_refuses_verbosity_out_of_range() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        // Anything above 2 is out of range: a parameter error naming what is served.
        assert!(matches!(
            node.get_block(&"11".repeat(32), Some(3)).await,
            Err(RpcError::InvalidParams(_))
        ));
    }

    #[tokio::test]
    async fn get_block_reports_an_unknown_block_as_not_found() {
        // Nothing scripted: all three reads miss, which is a not-found error.
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_block("999999", Some(2)).await,
            Err(RpcError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_block_never_renders_a_partial_block() {
        // Only the chain position is scripted, not the block or its transactions:
        // the live passthrough reads disagree, so the handler errors rather than
        // rendering a block with a defaulted body.
        let (_, verbose) = scripted_block_and_verbose();
        let engine = MockIndexerService::new(MockChain {
            block: None,
            block_verbose: Some(verbose),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        match node.get_block("2468", Some(2)).await {
            Err(RpcError::Read(_)) => {}
            other => panic!("a half-present block must error, not render: {other:?}"),
        }
    }

    #[test]
    fn block_selector_parses_height_digits_and_hash_hex() {
        use zaino_primitives::types::BlockSelector;
        assert_eq!(
            block_selector_from_str("2468").expect("height"),
            BlockSelector::Height(Height::try_from(2_468).expect("valid height"))
        );
        // A real hash contains hex letters, so it is never mistaken for a height.
        assert_eq!(
            block_selector_from_str(&"ab".repeat(32)).expect("hash"),
            BlockSelector::Hash(BlockHash::from([0xab; 32]))
        );
        // Not all-digits and not 32-byte hex: a params error.
        assert!(matches!(
            block_selector_from_str("nothex"),
            Err(RpcError::InvalidParams(_))
        ));
        assert!(matches!(
            block_selector_from_str(&"ab".repeat(31)),
            Err(RpcError::InvalidParams(_))
        ));
    }

    #[tokio::test]
    async fn block_header_renders_the_scripted_header() {
        use zaino_primitives::types::rpc::BlockHeaderVerbose;
        use zaino_primitives::types::{AbsoluteChainWork, CompactDifficulty};
        let mut work_bytes = [0u8; 32];
        work_bytes[28..].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let header = BlockHeaderVerbose {
            hash: BlockHash::from([0x11; 32]),
            confirmations: 7,
            height: Height::try_from(2_468).expect("valid height"),
            version: 4,
            merkle_root: [0x22; 32].into(),
            final_sapling_root: Some([0x33; 32].into()),
            time: 1_600_000_000,
            nonce: [0x44; 32],
            solution: vec![0xaa, 0xbb, 0xcc],
            bits: CompactDifficulty::try_from_bits(0x1f07_ffff).expect("valid nBits"),
            difficulty: 123.5,
            block_commitments: Some([0x55; 32].into()),
            chainwork: AbsoluteChainWork::try_from_reported(work_bytes).expect("in-range work"),
            previous_block_hash: Some(BlockHash::from([0x66; 32])),
            next_block_hash: Some(BlockHash::from([0x77; 32])),
        };
        let engine = MockIndexerService::new(MockChain {
            block_header_verbose: Some(header),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        let got = node
            .get_block_header(&"11".repeat(32))
            .await
            .expect("header served");
        assert_eq!(got.hash, "11".repeat(32));
        assert_eq!(got.confirmations, 7);
        assert_eq!(got.height, 2_468);
        assert_eq!(got.merkle_root, "22".repeat(32));
        assert_eq!(got.bits, "1f07ffff");
        assert_eq!(
            got.next_block_hash.as_deref(),
            Some("77".repeat(32).as_str())
        );
    }

    #[tokio::test]
    async fn block_header_reports_an_unknown_hash_as_not_found() {
        // No scripted header: the read answers `Ok(None)`, which the handler
        // turns into a not-found RPC error, never a defaulted header.
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_block_header(&"ab".repeat(32)).await,
            Err(RpcError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn block_header_rejects_a_wrong_length_hash_at_the_boundary() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        for bad in [&"ab".repeat(31), &"ab".repeat(33), "xyz"] {
            assert!(matches!(
                node.get_block_header(bad).await,
                Err(RpcError::InvalidParams(_))
            ));
        }
    }

    #[tokio::test]
    async fn address_balance_renders_the_scripted_balance() {
        use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum};
        let engine = MockIndexerService::new(MockChain {
            // A tip makes the chain serviceable, so the scripted balance is read.
            tip: Some(BlockRef {
                height: Height::try_from(10).expect("valid height"),
                hash: BlockHash::from([0u8; 32]),
            }),
            balances: vec![(
                "t1abc".to_string(),
                zaino_primitives::types::AddressBalance {
                    balance: Zatoshis::new(5_000).expect("valid amount"),
                    received: ZatoshisFlowSum::from_summed(12_000),
                },
            )],
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        let got = node
            .get_address_balance(crate::wire::params::AddressesParam {
                addresses: vec!["t1abc".to_string()],
            })
            .await
            .expect("balance");
        assert_eq!(got.balance, 5_000);
        assert_eq!(got.received, 12_000);
    }

    /// With nothing serviceable (no tip, hence no coverage), the handler answers
    /// an empty total without querying the read — even when a balance is
    /// scripted for the address. Fails if `full_range` synthesises a range for a
    /// chain that serves nothing, since the mock ignores the range and would
    /// then return the scripted value.
    #[tokio::test]
    async fn address_balance_is_zero_when_nothing_is_serviceable() {
        use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum};
        let engine = MockIndexerService::new(MockChain {
            tip: None,
            balances: vec![(
                "t1abc".to_string(),
                zaino_primitives::types::AddressBalance {
                    balance: Zatoshis::new(5_000).expect("valid amount"),
                    received: ZatoshisFlowSum::from_summed(12_000),
                },
            )],
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        let got = node
            .get_address_balance(crate::wire::params::AddressesParam {
                addresses: vec!["t1abc".to_string()],
            })
            .await
            .expect("an unserviceable chain is a valid query with an empty answer");
        assert_eq!(got.balance, 0);
        assert_eq!(got.received, 0);
    }

    /// Review Focus 2: on a serviceable chain, an address absent from history is
    /// zero, not an error — the read's domain miss, distinct from the
    /// nothing-serviceable case above.
    #[tokio::test]
    async fn an_address_with_no_history_is_zero_not_an_error() {
        let tip = BlockRef {
            height: Height::try_from(10).expect("valid height"),
            hash: BlockHash::from([0u8; 32]),
        };
        let node = NodeRpc::new(engine_with_tip(Some(tip)), Network::MainNetwork);
        let got = node
            .get_address_balance(crate::wire::params::AddressesParam {
                addresses: vec!["t1nohistory".to_string()],
            })
            .await
            .expect("an unknown address is a valid query");
        assert_eq!(got.balance, 0);
        assert_eq!(got.received, 0);
    }

    #[tokio::test]
    async fn address_balance_rejects_an_empty_address_list() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_address_balance(crate::wire::params::AddressesParam {
                addresses: Vec::new()
            })
            .await,
            Err(RpcError::InvalidParams(_))
        ));
    }

    /// `chainInfo` is a wire choice: it decides whether the queried range is
    /// echoed, not what gets queried.
    #[tokio::test]
    async fn chain_info_decides_whether_the_range_is_echoed() {
        use zaino_primitives::types::{SignedZatoshis, TransparentAddress};
        let scripted = zaino_primitives::types::AddressDelta {
            satoshis: SignedZatoshis::try_new(-3).expect("valid delta"),
            txid: TransactionId::from([7u8; 32]),
            index: 0,
            height: Height::try_from(150).expect("valid height"),
            address: TransparentAddress::new("t1a".to_string()),
            block_index: Some(1),
        };
        let chain = MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(200).expect("valid height"),
                hash: BlockHash::from([1u8; 32]),
            }),
            deltas: vec![scripted],
            ..Default::default()
        };
        let node = NodeRpc::new(MockIndexerService::new(chain), Network::MainNetwork);

        let with = node
            .get_address_deltas(crate::wire::params::AddressDeltasParam {
                addresses: vec!["t1a".to_string()],
                start: None,
                end: None,
                chain_info: true,
            })
            .await
            .expect("deltas");
        assert_eq!(with.deltas.len(), 1);
        assert_eq!(with.deltas[0].satoshis, -3);
        assert!(with.range.is_some());

        let without = node
            .get_address_deltas(crate::wire::params::AddressDeltasParam {
                addresses: vec!["t1a".to_string()],
                start: None,
                end: None,
                chain_info: false,
            })
            .await
            .expect("deltas");
        assert_eq!(without.deltas.len(), 1, "the query is the same either way");
        assert!(without.range.is_none());
    }

    #[tokio::test]
    async fn address_deltas_rejects_an_empty_address_list() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_address_deltas(crate::wire::params::AddressDeltasParam {
                addresses: Vec::new(),
                start: None,
                end: None,
                chain_info: false,
            })
            .await,
            Err(RpcError::InvalidParams(_))
        ));
    }

    #[tokio::test]
    async fn raw_transaction_returns_the_scripted_hex() {
        use zaino_primitives::types::{RawTransaction, TransactionLocation};
        let txid = TransactionId::from([0xABu8; 32]);
        let engine = MockIndexerService::new(MockChain {
            raw_transactions: vec![(
                txid,
                RawTransaction {
                    data: vec![0xDE, 0xAD, 0xBE, 0xEF],
                    location: TransactionLocation::BestChain(
                        Height::try_from(42).expect("valid height"),
                    ),
                },
            )],
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        match node
            .get_raw_transaction(&"ab".repeat(32), Some(0))
            .await
            .expect("raw tx")
        {
            GetRawTransactionResponse::Raw(hex) => assert_eq!(hex, "deadbeef"),
            other => panic!("verbosity 0 is the raw hex: {other:?}"),
        }
    }

    #[tokio::test]
    async fn raw_transaction_reports_an_unknown_txid_as_not_found() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_raw_transaction(&"cd".repeat(32), Some(0)).await,
            Err(RpcError::NotFound(_))
        ));
    }

    /// A verbosity above 1 is out of range — a parameter error naming what is
    /// served, not a silently truncated answer.
    #[tokio::test]
    async fn raw_transaction_refuses_verbosity_above_one() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_raw_transaction(&"ab".repeat(32), Some(2)).await,
            Err(RpcError::InvalidParams(_))
        ));
    }

    /// A located view, for the verbosity-1 location tests.
    fn located_view(
        location: zaino_primitives::types::TransactionLocation,
    ) -> zaino_service::LocatedTransactionView {
        use zaino_primitives::types::{
            OrchardData, SaplingData, TransactionDetail, TransparentData,
        };
        use zaino_service::{LocatedTransactionView, TransactionView};
        let transaction = zaino_primitives::types::Transaction {
            txid: TransactionId::from([0xAB; 32]),
            transparent: TransparentData::default(),
            sapling: SaplingData::default(),
            orchard: OrchardData::default(),
            ironwood: OrchardData::default(),
        };
        let detail = TransactionDetail {
            version: 5,
            overwintered: true,
            version_group_id: Some(0x26a7_270a),
            lock_time: 0,
            expiry_height: Some(Height::try_from(0).expect("valid height")),
            size: 120,
            coinbase: None,
            joinsplits: Vec::new(),
        };
        LocatedTransactionView {
            view: TransactionView {
                transaction,
                detail,
                inputs: Vec::new(),
            },
            location,
        }
    }

    /// A mempool transaction has no containing block, so verbosity 1 emits none
    /// of the chain-location keys — they are absent, not null or zero.
    #[tokio::test]
    async fn raw_transaction_verbose_omits_location_for_a_mempool_tx() {
        use zaino_primitives::types::TransactionLocation;
        let engine = MockIndexerService::new(MockChain {
            transaction_view: Some(located_view(TransactionLocation::Mempool)),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        let response = node
            .get_raw_transaction(&"ab".repeat(32), Some(1))
            .await
            .expect("verbose tx");
        let json = serde_json::to_value(&response).expect("serialize");
        let obj = json.as_object().expect("a JSON object");
        for absent in ["height", "confirmations", "blockhash", "time", "blocktime"] {
            assert!(
                !obj.contains_key(absent),
                "a mempool tx omits {absent}, it is not rendered as null or zero"
            );
        }
        // The transaction object itself is still present.
        assert_eq!(
            obj.get("txid").and_then(serde_json::Value::as_str),
            Some("ab".repeat(32).as_str())
        );
    }

    /// A mined transaction carries its location: the height and confirmations
    /// from the tip, and the block hash and time from the containing block.
    #[tokio::test]
    async fn raw_transaction_verbose_renders_location_for_a_mined_tx() {
        use zaino_primitives::types::TransactionLocation;
        let (block, _) = scripted_block_and_verbose();
        let engine = MockIndexerService::new(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(2_470).expect("valid height"),
                hash: BlockHash::from([0x11; 32]),
            }),
            block: Some(block),
            transaction_view: Some(located_view(TransactionLocation::BestChain(
                Height::try_from(2_468).expect("valid height"),
            ))),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        let response = node
            .get_raw_transaction(&"ab".repeat(32), Some(1))
            .await
            .expect("verbose tx");
        let json = serde_json::to_value(&response).expect("serialize");
        let obj = json.as_object().expect("a JSON object");
        assert_eq!(
            obj.get("height").and_then(serde_json::Value::as_u64),
            Some(2_468)
        );
        // tip 2470 - height 2468 + 1 = 3 confirmations.
        assert_eq!(
            obj.get("confirmations").and_then(serde_json::Value::as_i64),
            Some(3)
        );
        assert_eq!(
            obj.get("blockhash").and_then(serde_json::Value::as_str),
            Some("11".repeat(32).as_str())
        );
        assert_eq!(
            obj.get("time").and_then(serde_json::Value::as_u64),
            Some(1_600_000_000)
        );
        assert_eq!(
            obj.get("blocktime").and_then(serde_json::Value::as_u64),
            Some(1_600_000_000)
        );
    }

    /// Review Focus 4: well-formed hex of the wrong length never reaches a read.
    #[tokio::test]
    async fn a_wrong_length_txid_is_rejected_at_the_boundary() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        for bad in [&"ab".repeat(31), &"ab".repeat(33)] {
            assert!(matches!(
                node.get_raw_transaction(bad, Some(0)).await,
                Err(RpcError::InvalidParams(_))
            ));
        }
    }

    /// Review Focus 5: garbage is `isvalid: false`, never an error. zcashd
    /// answers rather than failing, and the explorer's search box relies on it.
    #[tokio::test]
    async fn validate_address_reports_garbage_as_invalid_not_an_error() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        let got = node
            .validate_address("definitely not an address")
            .await
            .expect("validation answers, it does not fail");
        assert!(!got.isvalid);
        assert!(got.address.is_none());
    }

    #[tokio::test]
    async fn z_validate_address_reports_garbage_as_invalid_not_an_error() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        let got = node
            .z_validate_address("definitely not an address")
            .await
            .expect("validation answers, it does not fail");
        assert!(!got.isvalid);
    }

    /// A non-unified address is a parameter error, not an empty result: the
    /// caller asked about a specific string, and answering "no receivers" would
    /// imply a valid unified address that bundles nothing.
    #[tokio::test]
    async fn listing_receivers_of_a_non_unified_address_is_a_params_error() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.z_list_unified_receivers("t1notunified").await,
            Err(RpcError::InvalidParams(_))
        ));
    }
}
