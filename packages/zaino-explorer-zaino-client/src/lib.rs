//! The explorer's one production [`ChainReader`]: zaino-noderpc's generated
//! jsonrpsee client, talking to a live zainod over the wire. Plain and
//! direct — no local caching, no retry policy, nothing beyond what the
//! generated client already gives for free.
#![forbid(unsafe_code)]

use jsonrpsee::http_client::HttpClient;
use zaino_explorer_domain::{
    AddressSummary, BlockDetail, BlockSummary, ChainReadError, ChainReader, NodeStatus, SpendInfo,
    TransactionDetail, TransactionOutput,
};
use zaino_noderpc::wire::params::{AddressTxidsParam, AddressesParam, GetSpentInfoParam};
use zaino_noderpc::wire::response::{GetBlockResponse, GetRawTransactionResponse};
use zaino_noderpc::NodeRpcApiClient;

/// An invariant violation: `getblock` was asked for verbosity 1 and answered
/// with a different shape. Not a real-world failure mode against a correct
/// server — kept as a typed error rather than a panic so a misbehaving
/// server degrades to an error, not a crash.
#[derive(Debug, thiserror::Error)]
#[error("getblock verbosity=1 returned an unexpected response shape")]
struct UnexpectedBlockVerbosity;

/// An invariant violation: `getrawtransaction` was asked for verbosity 1 and
/// answered with the raw-hex shape instead.
#[derive(Debug, thiserror::Error)]
#[error("getrawtransaction verbosity=1 returned an unexpected response shape")]
struct UnexpectedTransactionVerbosity;

/// Wraps a [`HttpClient`] built against zaino-noderpc's generated
/// `NodeRpcApiClient`.
#[derive(Clone)]
pub struct ZainoClient(HttpClient);

impl ZainoClient {
    /// Wrap an already-built [`HttpClient`] pointed at a live zainod.
    pub fn new(client: HttpClient) -> Self {
        Self(client)
    }
}

impl ZainoClient {
    /// One block's summary via `getblock` at verbosity 1 — hash, height,
    /// time and transaction count in a single call, no `getblockhash` +
    /// `getblockheader` round trip (and no dependency on `getblockhash`
    /// being served at all, which not every deployed zainod does).
    async fn block_summary(&self, height: u32) -> Result<BlockSummary, ChainReadError> {
        let response = self
            .0
            .block(height.to_string(), Some(1))
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        block_summary_from_response(response)
    }
}

/// Map a `getblock` verbosity-1 response to a [`BlockSummary`]. A pure
/// function so the mapping is unit-testable without a server.
fn block_summary_from_response(response: GetBlockResponse) -> Result<BlockSummary, ChainReadError> {
    match response {
        GetBlockResponse::Verbose1(block) => Ok(BlockSummary {
            height: block.height,
            hash: block.hash,
            time: block.time,
            tx_count: block.n_tx,
        }),
        GetBlockResponse::Raw(_) | GetBlockResponse::Verbose2(_) => {
            Err(ChainReadError::Rpc(Box::new(UnexpectedBlockVerbosity)))
        }
    }
}

/// Map a `getblock` verbosity-1 response to a [`BlockDetail`] — the same
/// response `block_summary_from_response` reads, kept as a separate pure
/// function rather than built on top of it so each caller only pays for the
/// fields it needs (a block list has no use for every txid).
fn block_detail_from_response(response: GetBlockResponse) -> Result<BlockDetail, ChainReadError> {
    match response {
        GetBlockResponse::Verbose1(block) => Ok(BlockDetail {
            height: block.height,
            hash: block.hash,
            time: block.time,
            tx_ids: block.tx,
        }),
        GetBlockResponse::Raw(_) | GetBlockResponse::Verbose2(_) => {
            Err(ChainReadError::Rpc(Box::new(UnexpectedBlockVerbosity)))
        }
    }
}

/// Map a `getrawtransaction` verbosity-1 response to a [`TransactionDetail`].
/// A pure function so the mapping is unit-testable without a server.
fn transaction_detail_from_response(
    response: GetRawTransactionResponse,
) -> Result<TransactionDetail, ChainReadError> {
    match response {
        GetRawTransactionResponse::Verbose(tx) => Ok(TransactionDetail {
            txid: tx.transaction.txid.clone(),
            size: tx.transaction.size,
            height: tx.height,
            confirmations: tx.confirmations,
            outputs: tx
                .transaction
                .vout
                .iter()
                .map(|output| TransactionOutput {
                    value_zat: output.value_zat,
                    addresses: output.script_pub_key.addresses.clone().unwrap_or_default(),
                })
                .collect(),
        }),
        GetRawTransactionResponse::Raw(_) => Err(ChainReadError::Rpc(Box::new(
            UnexpectedTransactionVerbosity,
        ))),
    }
}

impl ChainReader for ZainoClient {
    async fn chain_height(&self) -> Result<u32, ChainReadError> {
        self.0
            .block_count()
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))
    }

    async fn recent_blocks(&self, count: u32) -> Result<Vec<BlockSummary>, ChainReadError> {
        let height = self.chain_height().await?;
        let oldest = height.saturating_sub(count.saturating_sub(1));
        let mut summaries = Vec::new();
        for h in (oldest..=height).rev() {
            summaries.push(self.block_summary(h).await?);
        }
        Ok(summaries)
    }

    async fn block(&self, height_or_hash: String) -> Result<BlockDetail, ChainReadError> {
        let response = self
            .0
            .block(height_or_hash, Some(1))
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        block_detail_from_response(response)
    }

    async fn transaction(&self, txid: String) -> Result<TransactionDetail, ChainReadError> {
        let response = self
            .0
            .raw_transaction(txid, Some(1))
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        transaction_detail_from_response(response)
    }

    async fn spend_info(
        &self,
        txid: String,
        output_index: u32,
    ) -> Result<SpendInfo, ChainReadError> {
        let response = self
            .0
            .spent_info(GetSpentInfoParam {
                txid,
                index: output_index,
            })
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        Ok(SpendInfo {
            spending_txid: response.txid,
            spending_input_index: response.index,
            height: response.height,
        })
    }

    async fn address(&self, address: String) -> Result<AddressSummary, ChainReadError> {
        let balance = self
            .0
            .address_balance(AddressesParam {
                addresses: vec![address.clone()],
            })
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        // Balance and tx history are independent RPCs, and not every
        // deployed zainod serves both (getaddresstxids is a newer addition
        // than getaddressbalance) — a txids failure degrades to an empty
        // list rather than hiding a balance the node can actually answer,
        // the same policy spend_info already uses for its own fallback.
        let txids = self
            .0
            .address_txids(AddressTxidsParam {
                addresses: vec![address.clone()],
                start: None,
                end: None,
            })
            .await
            .unwrap_or_default();
        Ok(AddressSummary {
            address,
            balance_zat: balance.balance,
            received_zat: balance.received,
            txids,
        })
    }

    async fn node_status(&self) -> Result<NodeStatus, ChainReadError> {
        let info = self
            .0
            .info()
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        let mempool = self
            .0
            .mempool_info()
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        Ok(NodeStatus {
            subversion: info.subversion,
            connections: info.connections,
            mempool_size: mempool.size,
            mempool_bytes: mempool.bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        block_detail_from_response, block_summary_from_response, transaction_detail_from_response,
        ZainoClient,
    };
    use jsonrpsee::http_client::HttpClientBuilder;
    use std::net::TcpListener;
    use zaino_explorer_domain::ChainReader;
    use zaino_noderpc::wire::response::{
        BlockResponse, GetBlockResponse, GetRawTransactionResponse, OrchardObject,
        RawTransactionResponse, ScriptPubKey, TransactionObject, TransactionOutput,
    };
    use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
    use zaino_primitives::types::{
        Block, BlockHash, BlockHeader, BlockTreeSizes, BlockVerbose, ChainMetadata,
        CompactDifficulty, DecodedBlock, EquihashSolution, Height,
    };
    use zaino_service::testing::{MockChain, MockIndexerService};
    use zcash_protocol::consensus::Network;

    /// Boot a real `NodeRpc` jsonrpsee server over the given mock chain, on an
    /// ephemeral port.
    fn spawn_mock_server(
        chain: MockChain,
    ) -> (std::net::SocketAddr, jsonrpsee::server::ServerHandle) {
        let handler = NodeRpc::new(MockIndexerService::new(chain), Network::MainNetwork);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.set_nonblocking(true).expect("set nonblocking");
        let addr = listener.local_addr().expect("local addr");
        let server = jsonrpsee::server::ServerBuilder::default()
            .build_from_tcp(listener)
            .expect("build server from listener");
        let handle = server.start(handler.into_rpc());
        (addr, handle)
    }

    /// A `getblock` verbosity-1 response maps field-for-field into a
    /// [`zaino_explorer_domain::BlockSummary`] — no server needed, this is
    /// pure mapping logic.
    #[test]
    fn verbose1_response_maps_to_block_summary() {
        let response = GetBlockResponse::Verbose1(BlockResponse {
            hash: "ab".repeat(32),
            confirmations: 5,
            height: 12_345,
            version: 4,
            merkle_root: String::new(),
            block_commitments: String::new(),
            final_sapling_root: None,
            final_orchard_root: None,
            n_tx: 7,
            time: 1_700_000_000,
            nonce: String::new(),
            solution: String::new(),
            bits: String::new(),
            difficulty: 1.0,
            chainwork: None,
            chain_supply: None,
            value_pools: Vec::new(),
            trees: zaino_noderpc::wire::response::TreesResponse {
                sapling: zaino_noderpc::wire::response::TreePoolSize { size: 0 },
                orchard: zaino_noderpc::wire::response::TreePoolSize { size: 0 },
                ironwood: zaino_noderpc::wire::response::TreePoolSize { size: 0 },
            },
            size: 1_000,
            previous_block_hash: None,
            next_block_hash: None,
            tx: Vec::<String>::new(),
        });

        let summary = block_summary_from_response(response).expect("maps ok");

        assert_eq!(summary.height, 12_345);
        assert_eq!(summary.hash, "ab".repeat(32));
        assert_eq!(summary.time, 1_700_000_000);
        assert_eq!(summary.tx_count, 7);
    }

    /// Any verbosity other than 1 is an adapter-side invariant violation
    /// (this client only ever requests verbosity 1) — a typed error, not a
    /// panic, so a misbehaving server degrades gracefully.
    #[test]
    fn non_verbose1_response_is_a_typed_error_not_a_panic() {
        let err = block_summary_from_response(GetBlockResponse::Raw("deadbeef".to_string()))
            .expect_err("wrong verbosity should error");
        let source = std::error::Error::source(&err).expect("Rpc variant carries a source");
        assert!(source.to_string().contains("unexpected"));
    }

    /// A `getblock` verbosity-1 response maps field-for-field into a
    /// [`zaino_explorer_domain::BlockDetail`], including every txid — the
    /// field a block list's `BlockSummary` doesn't carry.
    #[test]
    fn verbose1_response_maps_to_block_detail_with_txids() {
        let response = GetBlockResponse::Verbose1(BlockResponse {
            hash: "ab".repeat(32),
            confirmations: 5,
            height: 12_345,
            version: 4,
            merkle_root: String::new(),
            block_commitments: String::new(),
            final_sapling_root: None,
            final_orchard_root: None,
            n_tx: 2,
            time: 1_700_000_000,
            nonce: String::new(),
            solution: String::new(),
            bits: String::new(),
            difficulty: 1.0,
            chainwork: None,
            chain_supply: None,
            value_pools: Vec::new(),
            trees: zaino_noderpc::wire::response::TreesResponse {
                sapling: zaino_noderpc::wire::response::TreePoolSize { size: 0 },
                orchard: zaino_noderpc::wire::response::TreePoolSize { size: 0 },
                ironwood: zaino_noderpc::wire::response::TreePoolSize { size: 0 },
            },
            size: 1_000,
            previous_block_hash: None,
            next_block_hash: None,
            tx: vec!["cd".repeat(32), "ef".repeat(32)],
        });

        let detail = block_detail_from_response(response).expect("maps ok");

        assert_eq!(detail.height, 12_345);
        assert_eq!(detail.hash, "ab".repeat(32));
        assert_eq!(detail.time, 1_700_000_000);
        assert_eq!(detail.tx_ids, vec!["cd".repeat(32), "ef".repeat(32)]);
    }

    /// A minimal but complete verbose transaction, for the mapping tests
    /// below — one output, no shielded fields, an empty Orchard bundle (as
    /// zebra renders even a version-4 transaction).
    fn scripted_transaction_object() -> TransactionObject {
        TransactionObject {
            txid: "ab".repeat(32),
            version: 4,
            overwintered: true,
            version_group_id: Some("892f2085".to_string()),
            locktime: 0,
            expiry_height: Some(500_000),
            size: 250,
            hex: String::new(),
            vin: Vec::new(),
            vout: vec![TransactionOutput {
                value: 0.0001,
                value_zat: 10_000,
                n: 0,
                script_pub_key: ScriptPubKey {
                    asm: String::new(),
                    hex: String::new(),
                    required_signatures: Some(1),
                    addresses: Some(vec!["t1examplePayoutAddress".to_string()]),
                    script_type: Some("pubkeyhash".to_string()),
                },
            }],
            vjoinsplit: Vec::new(),
            value_balance: None,
            value_balance_zat: None,
            shielded_spends: None,
            shielded_outputs: None,
            orchard: OrchardObject {
                actions: Vec::new(),
                value_balance: 0.0,
                value_balance_zat: 0,
            },
            in_active_chain: Some(true),
        }
    }

    /// A `getrawtransaction` verbosity-1 response maps field-for-field into
    /// a [`zaino_explorer_domain::TransactionDetail`] — including each
    /// output's value and address — no server needed.
    #[test]
    fn verbose_response_maps_to_transaction_detail() {
        let response = GetRawTransactionResponse::Verbose(Box::new(RawTransactionResponse {
            transaction: scripted_transaction_object(),
            height: Some(12_345),
            confirmations: Some(5),
            blockhash: Some("cd".repeat(32)),
            time: Some(1_700_000_000),
            blocktime: Some(1_700_000_000),
        }));

        let detail = transaction_detail_from_response(response).expect("maps ok");

        assert_eq!(detail.txid, "ab".repeat(32));
        assert_eq!(detail.size, 250);
        assert_eq!(detail.height, Some(12_345));
        assert_eq!(detail.confirmations, Some(5));
        assert_eq!(detail.outputs.len(), 1);
        assert_eq!(detail.outputs[0].value_zat, 10_000);
        assert_eq!(detail.outputs[0].addresses, vec!["t1examplePayoutAddress"]);
    }

    /// The raw-hex (verbosity 0) shape is a typed error here too, not a
    /// panic — this client always requests verbosity 1.
    #[test]
    fn raw_transaction_response_is_a_typed_error_not_a_panic() {
        let err = transaction_detail_from_response(GetRawTransactionResponse::Raw(
            "deadbeef".to_string(),
        ))
        .expect_err("raw response should error");
        let source = std::error::Error::source(&err).expect("Rpc variant carries a source");
        assert!(source.to_string().contains("unexpected"));
    }

    /// A block header with distinguishable values, for a real end-to-end
    /// `getblock` round trip.
    fn scripted_header() -> BlockHeader {
        BlockHeader {
            hash: BlockHash::from([0x11; 32]),
            version: 4,
            prev_hash: BlockHash::from([0x22; 32]),
            height: Height::try_from(300).expect("valid height"),
            time: 1_700_000_300,
            merkle_root: [0x33; 32].into(),
            block_commitments: [0x44; 32].into(),
            bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
            nonce: [0x55; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        }
    }

    /// `recent_blocks(1)` against a real mock server: proof the whole wire
    /// path (client call -> server -> engine composition -> wire response ->
    /// our mapping) produces the right [`zaino_explorer_domain::BlockSummary`]
    /// for the one real implementor, not just hand-constructed state.
    //
    // multi_thread required: drives a live jsonrpsee server (its own accept
    // loop) concurrently with the outbound RPC call, on one runtime — same
    // justification as the sibling crates' identical tests.
    #[tokio::test(flavor = "multi_thread")]
    async fn recent_blocks_against_a_real_server() {
        let chain = MockChain {
            tip: Some(zaino_primitives::types::BlockRef {
                height: Height::try_from(300).expect("valid height"),
                hash: BlockHash::from([0x11; 32]),
            }),
            block: Some(Block {
                header: scripted_header(),
                transactions: Vec::new(),
                chain_metadata: ChainMetadata::ZERO,
            }),
            block_verbose: Some(BlockVerbose {
                confirmations: 1,
                difficulty: 1.0,
                chainwork: None,
                chain_supply: None,
                value_pools: Vec::new(),
                final_sapling_root: None,
                final_orchard_root: None,
                tree_sizes: BlockTreeSizes::default(),
                next_block_hash: None,
            }),
            decoded_block: Some(DecodedBlock {
                size: 1_000,
                transactions: Vec::new(),
            }),
            ..Default::default()
        };
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let blocks = reader.recent_blocks(1).await.expect("recent_blocks ok");

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].height, 300);
        assert_eq!(blocks[0].hash, "11".repeat(32));
        assert_eq!(blocks[0].time, 1_700_000_300);

        let _ = handle.stop();
    }

    /// `block` against a real mock server, by height: proof the whole wire
    /// path produces a [`zaino_explorer_domain::BlockDetail`] with its
    /// txids populated, not just the summary fields `recent_blocks` needs.
    #[tokio::test(flavor = "multi_thread")]
    async fn block_against_a_real_server() {
        let chain = MockChain {
            tip: Some(zaino_primitives::types::BlockRef {
                height: Height::try_from(300).expect("valid height"),
                hash: BlockHash::from([0x11; 32]),
            }),
            block: Some(Block {
                header: scripted_header(),
                transactions: Vec::new(),
                chain_metadata: ChainMetadata::ZERO,
            }),
            block_verbose: Some(BlockVerbose {
                confirmations: 1,
                difficulty: 1.0,
                chainwork: None,
                chain_supply: None,
                value_pools: Vec::new(),
                final_sapling_root: None,
                final_orchard_root: None,
                tree_sizes: BlockTreeSizes::default(),
                next_block_hash: None,
            }),
            decoded_block: Some(DecodedBlock {
                size: 1_000,
                transactions: Vec::new(),
            }),
            ..Default::default()
        };
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let detail = reader.block("300".to_string()).await.expect("block ok");

        assert_eq!(detail.height, 300);
        assert_eq!(detail.hash, "11".repeat(32));
        assert_eq!(detail.time, 1_700_000_300);

        let _ = handle.stop();
    }

    /// `spend_info` against a real mock server — the differentiator
    /// capability (Zebra itself does not serve `getspentinfo`).
    #[tokio::test(flavor = "multi_thread")]
    async fn spend_info_against_a_real_server() {
        use zaino_primitives::types::{Outpoint, TransactionId, TransparentSpend};

        let chain = MockChain {
            spend_info: Some(TransparentSpend {
                outpoint: Outpoint {
                    txid: TransactionId::from([0x01; 32]),
                    index: 0,
                },
                by: TransactionId::from([0x02; 32]),
                input_index: 1,
                height: Height::try_from(300).expect("valid height"),
                block_index: 0,
            }),
            ..Default::default()
        };
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let spend = reader
            .spend_info("01".repeat(32), 0)
            .await
            .expect("spend_info ok");

        assert_eq!(spend.spending_txid, "02".repeat(32));
        assert_eq!(spend.spending_input_index, 1);
        assert_eq!(spend.height, 300);

        let _ = handle.stop();
    }

    /// `address` against a real mock server: balance and txids, both
    /// fetched live — not a hand-rolled combination.
    #[tokio::test(flavor = "multi_thread")]
    async fn address_against_a_real_server() {
        use zaino_primitives::types::{
            AddressBalance, BlockHash, BlockRef, Height as PrimHeight, TransactionId, Zatoshis,
            ZatoshisFlowSum,
        };

        let chain = MockChain {
            // `address_balance`'s serviceability check reads `ChainSegment::
            // coverage`, which the mock derives from `tip` — not a
            // `serviceable` field (that one's for a different capability).
            tip: Some(BlockRef {
                height: PrimHeight::try_from(300).expect("valid height"),
                hash: BlockHash::from([0x09; 32]),
            }),
            balances: vec![(
                "t1exampleaddress".to_string(),
                AddressBalance {
                    balance: Zatoshis::new(5_000).expect("valid amount"),
                    received: ZatoshisFlowSum::from_summed(10_000u64),
                },
            )],
            txids: vec![TransactionId::from([0x03; 32])],
            ..Default::default()
        };
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let summary = reader
            .address("t1exampleaddress".to_string())
            .await
            .expect("address ok");

        assert_eq!(summary.address, "t1exampleaddress");
        assert_eq!(summary.balance_zat, 5_000);
        assert_eq!(summary.received_zat, 10_000);
        assert_eq!(summary.txids, vec!["03".repeat(32)]);

        let _ = handle.stop();
    }

    /// `node_status` against a real mock server: the mock has no validator
    /// behind it, so `getinfo` honestly answers "not ready" — this proves
    /// that failure surfaces as a typed `Err`, not a panic or a stale
    /// default. (A happy-path test would need a richer mock than this
    /// crate's `testing` module currently scripts for `NodeStatusRead`.)
    #[tokio::test(flavor = "multi_thread")]
    async fn node_status_surfaces_not_ready_as_an_error() {
        let chain = MockChain::default();
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let result = reader.node_status().await;

        assert!(result.is_err(), "an unready node should error, not panic");

        let _ = handle.stop();
    }
}
