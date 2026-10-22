//! Canned zebrad blocks through both clients → decoded block = block-explorer data
//!
//! - `tests/fixtures/block_<height>.hex` = zebrad's `getblock <height> 0` hex
//! - `ZcashLegacyRpcClient` = `getblock … 0` hex over JSON-RPC
//! - `ZebraIndexerGrpcClient` = same bytes over the indexer's `GetBlock` / `GetBlockRange`

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::{Request, Response, Status};

use zaino_primitives::types::{Block, Transaction};
use zaino_proto::proto::zebra_indexer::indexer_server::{Indexer, IndexerServer};
use zaino_proto::proto::zebra_indexer::{
    BlockAndHash, BlockHashAndHeight, BlockRangeRequest, BlockRequest, Empty, MempoolChangeMessage,
    NonFinalizedStateChangeRequest,
};
use zaino_source::testing::fixtures::{block, HEIGHTS};
use zaino_source::{
    Auth, ChainDataSource, Endpoint, FailureMode, GetAtHeightError, LinkLimits, QueryError,
    Timeouts, ZcashLegacyRpcClient, ZebraIndexerGrpcClient,
};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

/// What a block explorer shows for one block (counts summed over its transactions)
#[derive(Debug, PartialEq)]
struct Facts {
    height: u32,
    hash: String,
    prev_hash: String,
    time: u32,
    tx_count: usize,
    transparent_inputs: usize,
    transparent_outputs: usize,
    sapling_spends: usize,
    sapling_outputs: usize,
    orchard_actions: usize,
}

fn facts(block: &Block) -> Facts {
    let (header, txs) = (block.header(), block.transactions());
    let sum = |count: fn(&Transaction) -> usize| txs.iter().map(count).sum();
    Facts {
        height: u32::from(header.height),
        hash: header.hash.to_string(),
        prev_hash: header.prev_hash.to_string(),
        time: header.time,
        tx_count: txs.len(),
        transparent_inputs: sum(|tx| tx.transparent.inputs.len()),
        transparent_outputs: sum(|tx| tx.transparent.outputs.len()),
        sapling_spends: sum(|tx| tx.sapling.spends.len()),
        sapling_outputs: sum(|tx| tx.sapling.outputs.len()),
        orchard_actions: sum(|tx| tx.orchard.actions.len()),
    }
}

/// Block explorer's facts for each `fixtures::HEIGHTS` block
fn explorer() -> [Facts; 5] {
    // counts: [txs, transparent in, transparent out, sapling spends, sapling outputs, orchard actions]
    let facts = |height, hash: &str, prev_hash: &str, time, counts: [usize; 6]| {
        let [tx_count, transparent_inputs, transparent_outputs, sapling_spends, sapling_outputs, orchard_actions] =
            counts;
        Facts {
            height,
            hash: hash.to_owned(),
            prev_hash: prev_hash.to_owned(),
            time,
            tx_count,
            transparent_inputs,
            transparent_outputs,
            sapling_spends,
            sapling_outputs,
            orchard_actions,
        }
    };
    [
        facts(
            419_200,
            "00000000025a57200d898ac7f21e26bf29028bbe96ec46e05b2c17cc9db9e4f3",
            "00000000025c3b19eb08bbc0d74c0c5e798fcd58b38ecdcdda6b83e5c5945295",
            1_540_779_337,
            [1, 0, 2, 0, 0, 0],
        ),
        facts(
            1_000_000,
            "000000000062eff9ae053020017bfef24e521a2704c5ec9ead2a4608ac70fc7a",
            "0000000001512dea98f49c5890d45f03b755bfea6adb4b284aa3eb3aa46af377",
            1_602_206_541,
            [6, 6, 11, 0, 0, 0],
        ),
        facts(
            1_687_104,
            "0000000000d723156d9b65ffcf4984da7a19675ed7e2f06d9e5d5188af087bf8",
            "000000000162bdf56998ca7703adad22cdce21252f6354c077e82ccf434e761f",
            1_654_019_405,
            [1, 0, 4, 0, 0, 0],
        ),
        facts(
            2_000_000,
            "00000000010accaf2f87934765dc2e0bf4823a2b1ae2c1395b334acfce52ad68",
            "0000000000648d155ec973abb1bc0aa876cdc19b5ede56f32a3546a7eca4425d",
            1_677_602_242,
            [44, 53, 35, 5, 12, 44],
        ),
        facts(
            2_500_000,
            "00000000000a18c24ade5fe0a0a6dd2639fea4552098cb1181f016110d3b3ef6",
            "00000000011f1ded32bb36f0ec2a6a6e2912808380078b88430bfc5c00b8dd10",
            1_715_296_781,
            [2, 0, 4, 0, 0, 2],
        ),
    ]
}

/// One HTTP request's body, read until its `Content-Length` (`None` = closed first)
async fn request_body(stream: &mut TcpStream) -> Option<String> {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0u8; 64 * 1024];
        let n = stream.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
        request.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&request);
        let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
        let length: usize = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase().strip_prefix("content-length: ")?.parse().ok()
            })
            .expect("content-length");
        if body.len() >= length {
            return Some(body.to_owned());
        }
    }
}

/// Loopback zebrad JSON-RPC, single or batched: `getblock <id>` → `blocks[id]` hex, else `-8`
async fn serve_json_rpc(blocks: HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind json-rpc");
    let address = listener.local_addr().expect("local addr").to_string();
    let answer = Arc::new(move |call: &serde_json::Value| {
        let id = &call["id"];
        let asked = call["params"][0].as_str().unwrap_or_default();
        match blocks.get(asked).filter(|_| call["method"] == "getblock") {
            Some(raw) => {
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": const_hex::encode(raw) })
            }
            None => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -8, "message": "Block not found" },
            }),
        }
    });
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let answer = Arc::clone(&answer);
            tokio::spawn(async move {
                let Some(body) = request_body(&mut stream).await else { return };
                let request: serde_json::Value = serde_json::from_str(&body).expect("JSON-RPC");
                let reply = match request {
                    serde_json::Value::Array(calls) => calls.iter().map(|c| answer(c)).collect(),
                    call => answer(&call),
                };
                let reply = reply.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                    reply.len(),
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    address
}

type Blocks = Pin<Box<dyn Stream<Item = Result<BlockAndHash, Status>> + Send>>;

/// zebrad's indexer: `GetBlock` by display hash, `GetBlockRange` by height (each range recorded)
///
/// - height not held = `NOT_FOUND` mid-stream (as zebrad ends a range at its tip)
struct FakeIndexer {
    by_height: HashMap<u32, Vec<u8>>,
    by_hash: HashMap<[u8; 32], Vec<u8>>,
    ranges: Mutex<Vec<(u32, u32)>>,
}

#[tonic::async_trait]
impl Indexer for FakeIndexer {
    type ChainTipChangeStream =
        Pin<Box<dyn Stream<Item = Result<BlockHashAndHeight, Status>> + Send>>;
    type NonFinalizedStateChangeStream = Blocks;
    type MempoolChangeStream =
        Pin<Box<dyn Stream<Item = Result<MempoolChangeMessage, Status>> + Send>>;
    type GetBlockRangeStream = Blocks;

    async fn chain_tip_change(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<Self::ChainTipChangeStream>, Status> {
        Err(Status::unimplemented("blocks only"))
    }

    async fn non_finalized_state_change(
        &self,
        _: Request<NonFinalizedStateChangeRequest>,
    ) -> Result<Response<Self::NonFinalizedStateChangeStream>, Status> {
        Err(Status::unimplemented("blocks only"))
    }

    async fn mempool_change(
        &self,
        _: Request<Empty>,
    ) -> Result<Response<Self::MempoolChangeStream>, Status> {
        Err(Status::unimplemented("blocks only"))
    }

    async fn get_block(
        &self,
        request: Request<BlockRequest>,
    ) -> Result<Response<BlockAndHash>, Status> {
        let display: [u8; 32] = request
            .into_inner()
            .hash_or_height
            .try_into()
            .map_err(|_| Status::invalid_argument("hashes only here"))?;
        let data =
            self.by_hash.get(&display).ok_or_else(|| Status::not_found("block not found"))?;
        let block = BlockAndHash { hash: display.to_vec(), data: data.clone().into() };
        Ok(Response::new(block))
    }

    async fn get_block_range(
        &self,
        request: Request<BlockRangeRequest>,
    ) -> Result<Response<Self::GetBlockRangeStream>, Status> {
        let BlockRangeRequest { start, end } = request.into_inner();
        self.ranges.lock().expect("ranges").push((start, end));
        let mut served = Vec::new();
        for height in start..=end {
            match self.by_height.get(&height) {
                Some(data) => {
                    served.push(Ok(BlockAndHash { hash: Vec::new(), data: data.clone().into() }))
                }
                None => {
                    served.push(Err(Status::not_found(format!("no block at height {height}"))));
                    break;
                }
            }
        }
        Ok(Response::new(Box::pin(futures::stream::iter(served))))
    }
}

/// Indexer over `by_height` (by hash: each decodable block's own hash) + an empty JSON-RPC
async fn grpc_client(
    by_height: HashMap<u32, Vec<u8>>,
) -> (ZebraIndexerGrpcClient, Arc<FakeIndexer>) {
    let by_hash = by_height.values().filter_map(|raw| {
        let hash = zaino_source::decode_block(raw).ok()?.header().hash.to_string();
        let display: [u8; 32] = const_hex::decode_to_array(hash).expect("display hex");
        Some((display, raw.clone()))
    });
    let by_hash = by_hash.collect();
    let indexer = Arc::new(FakeIndexer { by_height, by_hash, ranges: Mutex::default() });
    let socket = TcpListener::bind("127.0.0.1:0").await.expect("bind fake indexer");
    let indexer_address = socket.local_addr().expect("local addr").to_string();
    let server = tonic::transport::Server::builder()
        .add_service(IndexerServer::from_arc(Arc::clone(&indexer)))
        .serve_with_incoming(tonic::transport::server::TcpIncoming::from(socket));
    tokio::spawn(server);
    let rpc_address = serve_json_rpc(HashMap::new()).await;
    let (timeouts, limits) = (Timeouts::default(), LinkLimits::default());
    let endpoint = Endpoint { address: rpc_address, auth: Auth::None, timeouts, limits };
    let legacy = ZcashLegacyRpcClient::new(&endpoint).expect("host:port");
    let grpc = ZebraIndexerGrpcClient::new(legacy, &indexer_address, None, timeouts, limits);
    (grpc.expect("host:port"), indexer)
}

/// Each fixture by hash and by height: one decoder, the explorer's facts
async fn decodes_each_fixture_to_the_explorers_facts(client: &impl ChainDataSource, label: &str) {
    for expected in explorer() {
        let height = BlockHeight::from(expected.height);
        let hash = BlockHash::from_hex(&expected.hash).expect("display hex");

        let by_hash = client.get_block_by_hash(hash).await;
        let by_height = client.get_blocks_by_height(&[height]).await;

        let by_hash = by_hash.unwrap_or_else(|e| panic!("{label}: block {hash} failed: {e}"));
        assert_eq!(facts(&by_hash), expected, "{label}: by hash at {height}");
        let by_height = by_height.unwrap_or_else(|e| panic!("{label}: block at {height}: {e}"));
        let by_height: Vec<Facts> = by_height.iter().map(facts).collect();
        assert_eq!(by_height, [expected], "{label}: by height {height}");
    }
}

#[tokio::test]
async fn the_json_rpc_client_decodes_every_fixture_to_the_explorers_facts() {
    let by_id = HEIGHTS.iter().flat_map(|&height| {
        let raw = block(height);
        let hash = zaino_source::decode_block(&raw).expect("fixture decodes").header().hash;
        [(height.to_string(), raw.clone()), (hash.to_string(), raw)]
    });
    let address = serve_json_rpc(by_id.collect()).await;
    let (timeouts, limits) = (Timeouts::default(), LinkLimits::default());
    let endpoint = Endpoint { address, auth: Auth::None, timeouts, limits };

    let client = ZcashLegacyRpcClient::new(&endpoint).expect("host:port");

    decodes_each_fixture_to_the_explorers_facts(&client, "json-rpc").await;
}

#[tokio::test]
async fn the_indexer_grpc_client_decodes_every_fixture_to_the_explorers_facts() {
    let by_height = HEIGHTS.iter().map(|&height| (height, block(height)));

    let (client, _) = grpc_client(by_height.collect()).await;

    decodes_each_fixture_to_the_explorers_facts(&client, "grpc").await;
}

/// Heights 10, 11, 12 serve fixtures 419 200, 1 000 000, 1 687 104 (decode only: no height check)
#[tokio::test]
async fn grpc_heights_are_fetched_one_range_per_consecutive_run_in_asked_order() {
    let served = [(10, 419_200), (11, 1_000_000), (12, 1_687_104), (2_000_000, 2_000_000)];
    let (client, indexer) =
        grpc_client(served.map(|(at, fixture)| (at, block(fixture))).into()).await;
    let h = BlockHeight::from;

    let blocks = client.get_blocks_by_height(&[h(10), h(11), h(12), h(2_000_000)]).await;

    let decoded: Vec<u32> =
        blocks.expect("all served").iter().map(|b| u32::from(b.header().height)).collect();
    assert_eq!(decoded, [419_200, 1_000_000, 1_687_104, 2_000_000], "fixtures, asked order");
    let ranges = indexer.ranges.lock().expect("ranges").clone();
    assert_eq!(ranges, [(10, 12), (2_000_000, 2_000_000)], "one GetBlockRange per run");
}

#[tokio::test]
async fn a_grpc_height_past_the_indexers_tip_is_not_found() {
    let (client, _) = grpc_client([(419_200, block(419_200))].into()).await;
    let h = BlockHeight::from;

    let past_tip = client.get_blocks_by_height(&[h(419_200), h(419_201)]).await;

    let past_tip = past_tip.map(|blocks| blocks.len());
    assert!(
        matches!(past_tip, Err(QueryError::Domain(GetAtHeightError::HeightNotFound(at))) if at == h(419_201)),
        "419 201 above the indexer's tip 419 200: {past_tip:?}"
    );
}

#[tokio::test]
async fn undecodable_grpc_bytes_are_a_parse_failure() {
    let (client, _) = grpc_client([(7, vec![0xff; 8])].into()).await;

    let corrupt = client.get_blocks_by_height(&[BlockHeight::from(7)]).await;

    let corrupt = corrupt.map(|blocks| blocks.len());
    assert!(
        matches!(&corrupt, Err(QueryError::NonDomain(error)) if error.mode == FailureMode::Parse),
        "8 bytes of 0xff at 7: {corrupt:?}"
    );
}
