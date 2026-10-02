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

use zaino_primitives::types::{Height, TransparentAddress};
use zaino_service::error::ReadError;
use zaino_service::queries;
use zaino_service::BlockVerboseRead;
use zaino_service::NodeQuery;
use zaino_service::RawTransactionRead;
use zaino_service::{BlockRead, ChainInfoRead, ChainSegment, NodeRpcService};
use zcash_protocol::consensus::Network;

use zaino_primitives::types::BlockSelector;

use crate::wire::params::{AddressDeltasParam, AddressesParam};
use crate::wire::response::{
    AddressBalanceResponse, AddressDeltasResponse, BlockHeaderResponse, BlockResponse,
    BlockchainInfoResponse, DeltaRange, UnifiedReceiversResponse, ValidateAddressResponse,
    ZValidateAddressResponse,
};
use crate::wire::{
    address_balance_to_wire, block_header_to_wire, block_to_wire, blockchain_info_to_wire,
    blockhash_from_hex, bytes_from_hex, bytes_to_hex, delta_to_wire, to_hex, txid_from_hex,
    unified_receivers_to_wire, validated_to_wire, z_validated_to_wire,
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

    /// `getrawtransaction`: the transaction's consensus bytes as hex.
    ///
    /// Only verbosity 0 is served here. The decoded form is a different
    /// capability — `TransactionRead`, which needs a verbose source port — so a
    /// verbose request is refused rather than answered with the raw shape.
    pub async fn get_raw_transaction(
        &self,
        txid_hex: &str,
        verbosity: Option<u32>,
    ) -> Result<String, RpcError> {
        match verbosity.unwrap_or(0) {
            0 => {}
            other => {
                return Err(RpcError::InvalidParams(format!(
                    "verbosity {other} is not served yet; only 0 (raw hex) is available"
                )))
            }
        }
        let txid = txid_from_hex(txid_hex)?;
        let snapshot = self.engine.snapshot().await?;
        let found = snapshot.raw_transaction(txid).await?;
        let tx = found
            .ok_or_else(|| RpcError::NotFound(format!("no transaction with id {txid_hex}")))?;
        Ok(bytes_to_hex(&tx.data))
    }

    /// `getblockchaininfo` (aggregate): reads the validator's `BlockchainInfo` —
    /// the node-rpc read delta the wallet-shaped ports lack — and renders it as
    /// zcashd's response. An unreachable validator surfaces as an RPC error
    /// (via `?`), never a response with defaulted fields.
    pub async fn get_blockchain_info(&self) -> Result<BlockchainInfoResponse, RpcError> {
        let snapshot = self.engine.snapshot().await?;
        let info = snapshot.chain_info().await?;
        Ok(blockchain_info_to_wire(info)?)
    }

    /// `getblock`: the full block object at verbosity 2 — the block page's read.
    ///
    /// The block id arrives as a string: all-digits is a height, otherwise a hex
    /// block hash (the explorer sends a height as a decimal string). Only
    /// verbosity 2 is served — the full block object with decoded transactions;
    /// any other verbosity is a params error naming what this method serves.
    ///
    /// The response composes two reads for the same block — its contents
    /// ([`BlockRead::block`]) and its chain position
    /// ([`BlockVerboseRead::block_verbose`]). Both missing is a not-found error;
    /// either read failing is an RPC error (via `?`), never a partially rendered
    /// block.
    pub async fn get_block(
        &self,
        blockid: &str,
        verbosity: Option<u32>,
    ) -> Result<BlockResponse, RpcError> {
        // zcashd defaults getblock to verbosity 1, which this method does not
        // serve, so an omitted verbosity is refused the same as any non-2.
        let requested = verbosity.unwrap_or(1);
        if requested != 2 {
            return Err(RpcError::InvalidParams(format!(
                "verbosity {requested} is not served; only 2 (the full block object with decoded transactions) is available"
            )));
        }
        let selector = block_selector_from_str(blockid)?;
        let snapshot = self.engine.snapshot().await?;
        let block = snapshot.block(selector).await.map_err(ReadError::from)?;
        let verbose = snapshot
            .block_verbose(selector)
            .await
            .map_err(ReadError::from)?;
        match (block, verbose) {
            (Some(block), Some(verbose)) => Ok(block_to_wire(block, verbose)),
            (None, None) => Err(RpcError::NotFound(format!("no block for {blockid}"))),
            // The two live passthrough reads disagree — a reorg race between
            // them. Not a partial render and not a definitive miss: transient.
            (Some(_), None) | (None, Some(_)) => Err(RpcError::Read(ReadError::Transient(
                format!("block {blockid} and its chain position disagree; retry"),
            ))),
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

    /// `getmininginfo`: not indexed — relayed to the validator through the
    /// passthrough seam and returned opaque.
    pub async fn get_mining_info(&self) -> Result<String, RpcError> {
        let answer = self.engine.relay_node_query(NodeQuery::MiningInfo).await?;
        Ok(answer.0)
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
    async fn chain_info_reads_and_mining_info_passes_through() {
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
        // Mining info: not indexed — relayed opaque through the passthrough seam.
        assert!(node
            .get_mining_info()
            .await
            .expect("mining info")
            .contains("MiningInfo"));
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

    #[tokio::test]
    async fn get_block_composes_the_block_and_its_chain_position() {
        let (block, verbose) = scripted_block_and_verbose();
        let engine = MockIndexerService::new(MockChain {
            block: Some(block),
            block_verbose: Some(verbose),
            ..Default::default()
        });
        let node = NodeRpc::new(engine, Network::MainNetwork);
        // A height arrives as a decimal string.
        let got = node.get_block("2468", Some(2)).await.expect("block served");
        assert_eq!(got.hash, "11".repeat(32));
        assert_eq!(got.height, 2_468);
        assert_eq!(got.confirmations, 9);
        assert_eq!(got.difficulty, 123.5);
        assert_eq!(got.tx.len(), 2);
    }

    #[tokio::test]
    async fn get_block_refuses_every_verbosity_but_two() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        // 0 and 1 and an omitted verbosity are all refused; the message names 2.
        for verbosity in [Some(0), Some(1), None, Some(3)] {
            assert!(matches!(
                node.get_block(&"11".repeat(32), verbosity).await,
                Err(RpcError::InvalidParams(_))
            ));
        }
    }

    #[tokio::test]
    async fn get_block_reports_an_unknown_block_as_not_found() {
        // Nothing scripted: both reads miss, which is a not-found error.
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_block("999999", Some(2)).await,
            Err(RpcError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_block_never_renders_a_partial_block() {
        // Only the chain position is scripted, not the block itself: the two live
        // passthrough reads disagree, so the handler errors rather than rendering
        // a block with a defaulted body.
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
        let got = node
            .get_raw_transaction(&"ab".repeat(32), Some(0))
            .await
            .expect("raw tx");
        assert_eq!(got, "deadbeef");
    }

    #[tokio::test]
    async fn raw_transaction_reports_an_unknown_txid_as_not_found() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_raw_transaction(&"cd".repeat(32), Some(0)).await,
            Err(RpcError::NotFound(_))
        ));
    }

    /// Verbosity 1 needs a capability this slice does not have. Refusing is
    /// honest; answering raw hex to a caller expecting the decoded object is not.
    #[tokio::test]
    async fn raw_transaction_refuses_verbose_until_the_capability_exists() {
        let node = NodeRpc::new(engine_with_tip(None), Network::MainNetwork);
        assert!(matches!(
            node.get_raw_transaction(&"ab".repeat(32), Some(1)).await,
            Err(RpcError::InvalidParams(_))
        ));
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
