//! The explorer's one production [`ChainReader`]: zaino-noderpc's generated
//! jsonrpsee client, talking to a live zainod over the wire. Plain and
//! direct — no local caching, no retry policy, nothing beyond what the
//! generated client already gives for free.
#![forbid(unsafe_code)]

use jsonrpsee::http_client::HttpClient;
use zaino_explorer_domain::{
    AddressSummary, BlockDeltas, BlockDetail, BlockSummary, ChainReadError, ChainReader,
    MempoolEntry, NodeStatus, PoolTreestate, SpendInfo, TransactionDelta, TransactionDetail,
    TransactionOutput, Treestate, ValueMovement,
};
use zaino_noderpc::wire::params::{AddressTxidsParam, AddressesParam, GetSpentInfoParam};
use zaino_noderpc::wire::response::{
    GetBlockDeltasResponse, GetBlockResponse, GetRawTransactionResponse, PoolTreestateResponse,
    RawMempoolResponse, TreestateResponse,
};
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

/// An invariant violation: `getblockdeltas` reported an output value that
/// does not fit in an `i64` — outside the invariant this adapter encodes
/// (every zatoshi quantity zcashd/zaino actually produce fits well within
/// that range). A typed error rather than a silent truncation or a panic.
#[derive(Debug, thiserror::Error)]
#[error("getblockdeltas output satoshis exceeds i64 range")]
struct OutputValueOutOfRange;

/// An invariant violation: `getrawmempool` was asked for `verbose=true` and
/// answered with the plain-txid shape instead.
#[derive(Debug, thiserror::Error)]
#[error("getrawmempool verbose=true returned an unexpected response shape")]
struct UnexpectedMempoolVerbosity;

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

/// Map a `getblockdeltas` response to a [`BlockDeltas`]. A pure function so
/// the mapping is unit-testable without a server. Fallible only on an
/// output value too large for `i64` — never observed in practice, but not
/// assumed away either.
fn block_deltas_from_response(
    response: GetBlockDeltasResponse,
) -> Result<BlockDeltas, ChainReadError> {
    let deltas = response
        .deltas
        .into_iter()
        .map(|delta| {
            let outputs = delta
                .outputs
                .into_iter()
                .map(|output| {
                    i64::try_from(output.satoshis)
                        .map(|value_zat| ValueMovement {
                            address: output.address,
                            value_zat,
                        })
                        .map_err(|_| ChainReadError::Rpc(Box::new(OutputValueOutOfRange)))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(TransactionDelta {
                txid: delta.txid,
                inputs: delta
                    .inputs
                    .into_iter()
                    .map(|input| ValueMovement {
                        address: input.address,
                        value_zat: input.satoshis,
                    })
                    .collect(),
                outputs,
            })
        })
        .collect::<Result<Vec<_>, ChainReadError>>()?;
    Ok(BlockDeltas {
        hash: response.hash,
        height: response.height,
        deltas,
    })
}

/// Map a `z_gettreestate` response to a [`Treestate`]. A pure function so
/// the mapping is unit-testable without a server. Infallible: every field
/// is a direct, same-shape copy.
fn treestate_from_response(response: TreestateResponse) -> Treestate {
    Treestate {
        hash: response.hash,
        height: response.height,
        time: response.time,
        sapling: response.sapling.map(pool_treestate_from_response),
        orchard: response.orchard.map(pool_treestate_from_response),
        ironwood: response.ironwood.map(pool_treestate_from_response),
    }
}

/// Map one pool's `z_gettreestate` entry to a [`PoolTreestate`].
fn pool_treestate_from_response(response: PoolTreestateResponse) -> PoolTreestate {
    PoolTreestate {
        final_root: response.commitments.final_root,
        final_state: response.commitments.final_state,
    }
}

/// Map a `getrawmempool` verbose response to a list of [`MempoolEntry`]s. A
/// pure function so the mapping is unit-testable without a server. This
/// client always requests `verbose=true`, so the non-verbose (plain txid
/// list) shape is an adapter-side invariant violation.
fn raw_mempool_from_response(
    response: RawMempoolResponse,
) -> Result<Vec<MempoolEntry>, ChainReadError> {
    match response {
        RawMempoolResponse::Verbose(entries) => Ok(entries
            .into_iter()
            .map(|(txid, entry)| MempoolEntry {
                txid,
                size: entry.size,
                fee_zat: entry.fee_zat,
                time: entry.time,
                height: entry.height,
            })
            .collect()),
        RawMempoolResponse::Txids(_) => {
            Err(ChainReadError::Rpc(Box::new(UnexpectedMempoolVerbosity)))
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

    async fn block_deltas(&self, hash: String) -> Result<BlockDeltas, ChainReadError> {
        let response = self
            .0
            .block_deltas(hash)
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        block_deltas_from_response(response)
    }

    async fn treestate(&self, height_or_hash: String) -> Result<Treestate, ChainReadError> {
        let response = self
            .0
            .z_treestate(height_or_hash)
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        Ok(treestate_from_response(response))
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

    async fn raw_mempool(&self) -> Result<Vec<MempoolEntry>, ChainReadError> {
        let response = self
            .0
            .raw_mempool(Some(true))
            .await
            .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
        raw_mempool_from_response(response)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        block_deltas_from_response, block_detail_from_response, block_summary_from_response,
        raw_mempool_from_response, transaction_detail_from_response, treestate_from_response,
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

    /// A `getblockdeltas` response maps field-for-field into a
    /// [`zaino_explorer_domain::BlockDeltas`] — a negative input value, a
    /// positive output value, and each movement's optional address, with no
    /// server needed.
    #[test]
    fn getblockdeltas_response_maps_to_block_deltas_with_signed_values() {
        use zaino_noderpc::wire::response::{
            GetBlockDeltasResponse, InputDeltaEntry, OutputDeltaEntry, TransactionDeltaEntry,
        };

        let response = GetBlockDeltasResponse {
            hash: "ab".repeat(32),
            confirmations: 5,
            size: 777,
            height: 100,
            version: 4,
            merkle_root: String::new(),
            deltas: vec![TransactionDeltaEntry {
                txid: "cd".repeat(32),
                index: 1,
                inputs: vec![InputDeltaEntry {
                    address: Some("t1spender".to_string()),
                    satoshis: -1_000,
                    index: 0,
                    prevtxid: "ef".repeat(32),
                    prevout: 2,
                }],
                outputs: vec![OutputDeltaEntry {
                    address: Some("t1receiver".to_string()),
                    satoshis: 600,
                    index: 0,
                }],
            }],
            time: 1_600_000_000,
            mediantime: 1_599_999_000,
            nonce: String::new(),
            bits: String::new(),
            difficulty: 1.0,
            chainwork: None,
            previous_block_hash: None,
            next_block_hash: None,
        };

        let deltas = block_deltas_from_response(response).expect("maps ok");

        assert_eq!(deltas.hash, "ab".repeat(32));
        assert_eq!(deltas.height, 100);
        assert_eq!(deltas.deltas.len(), 1);
        let tx = &deltas.deltas[0];
        assert_eq!(tx.txid, "cd".repeat(32));
        assert_eq!(tx.inputs[0].value_zat, -1_000);
        assert_eq!(tx.inputs[0].address, Some("t1spender".to_string()));
        assert_eq!(tx.outputs[0].value_zat, 600);
        assert_eq!(tx.outputs[0].address, Some("t1receiver".to_string()));
    }

    /// A `z_gettreestate` response maps field-for-field into a
    /// [`zaino_explorer_domain::Treestate`] — an active pool's root and
    /// state, and an inactive pool's absence, both preserved.
    #[test]
    fn z_gettreestate_response_maps_to_treestate() {
        use zaino_noderpc::wire::response::{
            CommitmentsResponse, PoolTreestateResponse, TreestateResponse,
        };

        let response = TreestateResponse {
            hash: "ab".repeat(32),
            height: 300,
            time: 1_700_000_300,
            sapling: Some(PoolTreestateResponse {
                commitments: CommitmentsResponse {
                    final_root: Some("cd".repeat(32)),
                    final_state: "deadbeef".to_string(),
                },
            }),
            orchard: None,
            ironwood: None,
        };

        let treestate = treestate_from_response(response);

        assert_eq!(treestate.hash, "ab".repeat(32));
        assert_eq!(treestate.height, 300);
        let sapling = treestate.sapling.expect("sapling active");
        assert_eq!(sapling.final_root, Some("cd".repeat(32)));
        assert_eq!(sapling.final_state, "deadbeef");
        assert!(treestate.orchard.is_none());
        assert!(treestate.ironwood.is_none());
    }

    /// A `getrawmempool` verbose response maps field-for-field into a list
    /// of [`zaino_explorer_domain::MempoolEntry`]s — no server needed.
    #[test]
    fn verbose_raw_mempool_response_maps_to_mempool_entries() {
        use std::collections::BTreeMap;
        use zaino_noderpc::wire::response::{MempoolEntryObject, RawMempoolResponse};

        let mut entries = BTreeMap::new();
        entries.insert(
            "ab".repeat(32),
            MempoolEntryObject {
                size: 250,
                fee: 0.00001,
                fee_zat: 1_000,
                time: Some(1_700_000_300),
                height: 300,
            },
        );

        let mempool =
            raw_mempool_from_response(RawMempoolResponse::Verbose(entries)).expect("maps ok");

        assert_eq!(mempool.len(), 1);
        assert_eq!(mempool[0].txid, "ab".repeat(32));
        assert_eq!(mempool[0].size, 250);
        assert_eq!(mempool[0].fee_zat, 1_000);
        assert_eq!(mempool[0].time, Some(1_700_000_300));
        assert_eq!(mempool[0].height, 300);
    }

    /// The non-verbose (plain txid list) shape is a typed error here, not a
    /// panic — this client always requests `verbose=true`.
    #[test]
    fn non_verbose_raw_mempool_response_is_a_typed_error_not_a_panic() {
        use zaino_noderpc::wire::response::RawMempoolResponse;

        let err = raw_mempool_from_response(RawMempoolResponse::Txids(vec!["ab".repeat(32)]))
            .expect_err("plain txid list should error");
        let source = std::error::Error::source(&err).expect("Rpc variant carries a source");
        assert!(source.to_string().contains("unexpected"));
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

    /// `block_deltas` against a real mock server — the differentiator
    /// capability (Zebra itself does not serve `getblockdeltas`). Proves
    /// the whole wire path produces signed [`zaino_explorer_domain::
    /// ValueMovement`]s, not just the pure-function mapping.
    #[tokio::test(flavor = "multi_thread")]
    async fn block_deltas_against_a_real_server() {
        use zaino_primitives::types::{
            BlockHash as PrimBlockHash, CompactDifficulty, Height as PrimHeight, Script,
            SignedZatoshis, TransactionId, Zatoshis,
        };
        use zaino_service::{InputDelta, OutputDelta, TransactionDeltas};

        let p2pkh = |b: u8| {
            let mut bytes = vec![0x76, 0xa9, 0x14];
            bytes.extend_from_slice(&[b; 20]);
            bytes.extend_from_slice(&[0x88, 0xac]);
            Script::new(bytes)
        };
        let spend = TransactionDeltas {
            txid: TransactionId::from([0x7A; 32]),
            index: 0,
            inputs: vec![InputDelta {
                script: p2pkh(0x02),
                satoshis: SignedZatoshis::try_new(-1_000).expect("valid amount"),
                index: 0,
                prev_txid: TransactionId::from([0xAB; 32]),
                prevout: 2,
            }],
            outputs: vec![OutputDelta {
                script: p2pkh(0x03),
                satoshis: Zatoshis::new(600).expect("valid amount"),
                index: 0,
            }],
        };
        let chain = MockChain {
            block_deltas: Some(zaino_service::BlockDeltas {
                hash: PrimBlockHash::from([0x11; 32]),
                confirmations: 1,
                size: 500,
                height: PrimHeight::try_from(300).expect("valid height"),
                version: 4,
                merkle_root: [0x22; 32].into(),
                deltas: vec![spend],
                time: 1_700_000_300,
                median_time: 1_700_000_000,
                nonce: [0x33; 32],
                bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
                difficulty: 1.0,
                chainwork: None,
                prev_hash: None,
                next_hash: None,
            }),
            ..Default::default()
        };
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let deltas = reader
            .block_deltas("11".repeat(32))
            .await
            .expect("block_deltas ok");

        assert_eq!(deltas.height, 300);
        assert_eq!(deltas.deltas.len(), 1);
        let tx = &deltas.deltas[0];
        assert_eq!(tx.inputs[0].value_zat, -1_000);
        assert_eq!(tx.outputs[0].value_zat, 600);
        assert!(tx.outputs[0].address.is_some());

        let _ = handle.stop();
    }

    /// `treestate` against a real mock server: proof the whole wire path
    /// produces a [`zaino_explorer_domain::Treestate`] with an active
    /// pool's root and an inactive pool's absence.
    #[tokio::test(flavor = "multi_thread")]
    async fn treestate_against_a_real_server() {
        use zaino_primitives::types::{
            BlockHash as PrimBlockHash, Height as PrimHeight, PoolTreestate, TreeRoot, Treestate,
        };

        let chain = MockChain {
            treestate: Some(Treestate {
                block_hash: PrimBlockHash::from([0x11; 32]),
                height: PrimHeight::try_from(300).expect("valid height"),
                time: 1_700_000_300,
                sapling: Some(PoolTreestate {
                    final_root: Some(TreeRoot::from([0x22; 32])),
                    final_state: vec![0xDE, 0xAD, 0xBE, 0xEF],
                }),
                orchard: None,
                ironwood: None,
            }),
            ..Default::default()
        };
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let treestate = reader
            .treestate("300".to_string())
            .await
            .expect("treestate ok");

        assert_eq!(treestate.height, 300);
        let sapling = treestate.sapling.expect("sapling active");
        assert_eq!(sapling.final_root, Some("22".repeat(32)));
        assert_eq!(sapling.final_state, "deadbeef");
        assert!(treestate.orchard.is_none());

        let _ = handle.stop();
    }

    /// `raw_mempool` against a real mock server: proof the whole wire path
    /// produces [`zaino_explorer_domain::MempoolEntry`]s, not just the
    /// pure-function mapping.
    #[tokio::test(flavor = "multi_thread")]
    async fn raw_mempool_against_a_real_server() {
        use zaino_primitives::types::{
            BlockHash as PrimBlockHash, BlockRef, Height as PrimHeight, TransactionId,
        };
        use zaino_service::MempoolTx;

        let chain = MockChain {
            mempool: vec![MempoolTx {
                txid: TransactionId::from([0x7A; 32]),
                validated_against: BlockRef {
                    height: PrimHeight::try_from(300).expect("valid height"),
                    hash: PrimBlockHash::from([0x11; 32]),
                },
            }],
            ..Default::default()
        };
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let mempool = reader.raw_mempool().await.expect("raw_mempool ok");

        assert_eq!(mempool.len(), 1);
        assert_eq!(mempool[0].txid, "7a".repeat(32));
        assert_eq!(mempool[0].height, 300);

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
