//! The explorer's one production [`ChainReader`]: zaino-noderpc's generated
//! jsonrpsee client, talking to a live zainod over the wire. Plain and
//! direct — no local caching, no retry policy, nothing beyond what the
//! generated client already gives for free.
#![forbid(unsafe_code)]

use jsonrpsee::http_client::HttpClient;
use zaino_explorer_domain::{BlockSummary, ChainReadError, ChainReader};
use zaino_noderpc::wire::response::GetBlockResponse;
use zaino_noderpc::NodeRpcApiClient;

/// An invariant violation: `getblock` was asked for verbosity 1 and answered
/// with a different shape. Not a real-world failure mode against a correct
/// server — kept as a typed error rather than a panic so a misbehaving
/// server degrades to an error, not a crash.
#[derive(Debug, thiserror::Error)]
#[error("getblock verbosity=1 returned an unexpected response shape")]
struct UnexpectedBlockVerbosity;

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
}

#[cfg(test)]
mod tests {
    use super::{block_summary_from_response, ZainoClient};
    use jsonrpsee::http_client::HttpClientBuilder;
    use std::net::TcpListener;
    use zaino_explorer_domain::ChainReader;
    use zaino_noderpc::wire::response::{BlockResponse, GetBlockResponse};
    use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
    use zaino_primitives::types::{
        Block, BlockHash, BlockHeader, BlockTreeSizes, BlockVerbose, ChainMetadata,
        CompactDifficulty, DecodedBlock, EquihashSolution, Height,
    };
    use zaino_service::testing::{MockChain, MockIndexerService};
    use zcash_protocol::consensus::Network;

    /// Boot a real `NodeRpc` jsonrpsee server over the given mock chain, on an
    /// ephemeral port.
    fn spawn_mock_server(chain: MockChain) -> (std::net::SocketAddr, jsonrpsee::server::ServerHandle) {
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
}
