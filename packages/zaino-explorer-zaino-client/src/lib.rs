//! The explorer's one production [`ChainReader`]: zaino-noderpc's generated
//! jsonrpsee client, talking to a live zainod over the wire. Plain and
//! direct — no local caching, no retry policy, nothing beyond what the
//! generated client already gives for free.
#![forbid(unsafe_code)]

use jsonrpsee::http_client::HttpClient;
use zaino_explorer_domain::{BlockSummary, ChainReadError, ChainReader};
use zaino_noderpc::NodeRpcApiClient;

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
            let hash = self
                .0
                .block_hash(h)
                .await
                .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
            let header = self
                .0
                .block_header(hash.clone())
                .await
                .map_err(|e| ChainReadError::Rpc(Box::new(e)))?;
            summaries.push(BlockSummary {
                height: h,
                hash,
                time: header.time,
            });
        }
        Ok(summaries)
    }
}

#[cfg(test)]
mod tests {
    use super::ZainoClient;
    use jsonrpsee::http_client::HttpClientBuilder;
    use std::net::TcpListener;
    use zaino_explorer_domain::ChainReader;
    use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
    use zaino_primitives::types::rpc::BlockHeaderVerbose;
    use zaino_primitives::types::{BlockHash, CompactDifficulty, Height, MerkleRoot};
    use zaino_service::testing::{MockChain, MockIndexerService};
    use zaino_service::BlockHashAt;
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

    /// `recent_blocks` walks the right height range, newest first, and each
    /// summary's hash comes from the per-height scripted lookup (not a single
    /// global value) — the mock's `block_hashes` is keyed by height, so three
    /// distinct heights here must come back as three distinct hashes.
    #[tokio::test(flavor = "multi_thread")]
    async fn recent_blocks_walks_height_range_newest_first() {
        let chain = MockChain {
            tip: Some(zaino_primitives::types::BlockRef {
                height: Height::try_from(300).expect("valid height"),
                hash: BlockHash::from([0xAAu8; 32]),
            }),
            block_hashes: vec![
                BlockHashAt {
                    height: Height::try_from(298).expect("valid height"),
                    hash: BlockHash::from([0x01u8; 32]),
                    time: 1_000,
                },
                BlockHashAt {
                    height: Height::try_from(299).expect("valid height"),
                    hash: BlockHash::from([0x02u8; 32]),
                    time: 2_000,
                },
                BlockHashAt {
                    height: Height::try_from(300).expect("valid height"),
                    hash: BlockHash::from([0x03u8; 32]),
                    time: 3_000,
                },
            ],
            block_header_verbose: Some(BlockHeaderVerbose {
                hash: BlockHash::from([0x03u8; 32]),
                confirmations: 1,
                height: Height::try_from(300).expect("valid height"),
                version: 4,
                merkle_root: MerkleRoot::from([0u8; 32]),
                time: 3_000,
                nonce: [0u8; 32],
                solution: Vec::new(),
                bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
                difficulty: 1.0,
                block_commitments: None,
                final_sapling_root: None,
                chainwork: None,
                previous_block_hash: None,
                next_block_hash: None,
            }),
            ..Default::default()
        };
        let (addr, handle) = spawn_mock_server(chain);
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let reader = ZainoClient::new(client);

        let blocks = reader.recent_blocks(3).await.expect("recent_blocks ok");

        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].height, 300, "newest first");
        assert_eq!(blocks[1].height, 299);
        assert_eq!(blocks[2].height, 298);
        // Each height's hash comes from the per-height scripted entry, not one
        // global value repeated three times.
        assert_ne!(blocks[0].hash, blocks[1].hash);
        assert_ne!(blocks[1].hash, blocks[2].hash);

        let _ = handle.stop();
    }
}
