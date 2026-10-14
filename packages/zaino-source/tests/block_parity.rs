//! Canned zebrad blocks through both clients → decoded block = block-explorer data
//!
//! - `tests/fixtures/block_<height>.hex` = zebrad's `getblock <height> 0` hex
//! - `ZcashLegacyRpcClient` = `getblock … 0` hex over JSON-RPC; `ZcashGrpcClient` = the same
//!   bytes over the indexer's `GetBlock` / `GetBlockRange`

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::{Request, Response, Status};

use zaino_primitives::types::BlockHash;
use zaino_proto::proto::zebra_indexer::indexer_server::{Indexer, IndexerServer};
use zaino_proto::proto::zebra_indexer::{
    BlockAndHash, BlockHashAndHeight, BlockRangeRequest, BlockRequest, Empty, MempoolChangeMessage,
    NonFinalizedStateChangeRequest,
};
use zaino_source::testing::fixtures::block;
use zaino_source::{
    ChainDataSource, FailureMode, GetAtHeightError, LinkLimits, QueryError, Timeouts,
    ZcashGrpcClient, ZcashLegacyRpcClient,
};
use zcash_protocol::consensus::BlockHeight;

struct Expected {
    height: u32,
    hash: &'static str,
    prev_hash: &'static str,
    time: u32,
    tx_count: usize,
    transparent_inputs: usize,
    transparent_outputs: usize,
    sapling_spends: usize,
    sapling_outputs: usize,
    orchard_actions: usize,
}

const FIXTURES: &[Expected] = &[
    Expected {
        height: 419_200,
        hash: "00000000025a57200d898ac7f21e26bf29028bbe96ec46e05b2c17cc9db9e4f3",
        prev_hash: "00000000025c3b19eb08bbc0d74c0c5e798fcd58b38ecdcdda6b83e5c5945295",
        time: 1_540_779_337,
        tx_count: 1,
        transparent_inputs: 0,
        transparent_outputs: 2,
        sapling_spends: 0,
        sapling_outputs: 0,
        orchard_actions: 0,
    },
    Expected {
        height: 1_000_000,
        hash: "000000000062eff9ae053020017bfef24e521a2704c5ec9ead2a4608ac70fc7a",
        prev_hash: "0000000001512dea98f49c5890d45f03b755bfea6adb4b284aa3eb3aa46af377",
        time: 1_602_206_541,
        tx_count: 6,
        transparent_inputs: 6,
        transparent_outputs: 11,
        sapling_spends: 0,
        sapling_outputs: 0,
        orchard_actions: 0,
    },
    Expected {
        height: 1_687_104,
        hash: "0000000000d723156d9b65ffcf4984da7a19675ed7e2f06d9e5d5188af087bf8",
        prev_hash: "000000000162bdf56998ca7703adad22cdce21252f6354c077e82ccf434e761f",
        time: 1_654_019_405,
        tx_count: 1,
        transparent_inputs: 0,
        transparent_outputs: 4,
        sapling_spends: 0,
        sapling_outputs: 0,
        orchard_actions: 0,
    },
    Expected {
        height: 2_000_000,
        hash: "00000000010accaf2f87934765dc2e0bf4823a2b1ae2c1395b334acfce52ad68",
        prev_hash: "0000000000648d155ec973abb1bc0aa876cdc19b5ede56f32a3546a7eca4425d",
        time: 1_677_602_242,
        tx_count: 44,
        transparent_inputs: 53,
        transparent_outputs: 35,
        sapling_spends: 5,
        sapling_outputs: 12,
        orchard_actions: 44,
    },
    Expected {
        height: 2_500_000,
        hash: "00000000000a18c24ade5fe0a0a6dd2639fea4552098cb1181f016110d3b3ef6",
        prev_hash: "00000000011f1ded32bb36f0ec2a6a6e2912808380078b88430bfc5c00b8dd10",
        time: 1_715_296_781,
        tx_count: 2,
        transparent_inputs: 0,
        transparent_outputs: 4,
        sapling_spends: 0,
        sapling_outputs: 0,
        orchard_actions: 2,
    },
];

/// Decimal height and display-hex hash → block hex
fn fixture_hex() -> HashMap<String, String> {
    let ids = |f: &Expected| {
        let hex = const_hex::encode(block(f.height));
        [(f.height.to_string(), hex.clone()), (f.hash.to_owned(), hex)]
    };
    FIXTURES.iter().flat_map(ids).collect()
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

/// Minimal HTTP server, single or batched JSON-RPC: `getblock <hash or height>` → canned fixture
/// hex, unknown → `-8`
async fn mock_zebra_rpc(listener: TcpListener, fixtures: HashMap<String, String>) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => return,
        };
        let fixtures = fixtures.clone();
        tokio::spawn(async move {
            let Some(body) = request_body(&mut stream).await else { return };
            let req: serde_json::Value =
                serde_json::from_str(&body).expect("invalid JSON-RPC request");

            let reply = match req {
                serde_json::Value::Array(calls) => serde_json::Value::Array(
                    calls.iter().map(|call| answer(call, &fixtures)).collect(),
                ),
                call => answer(&call, &fixtures),
            };
            let resp_body = serde_json::to_string(&reply).expect("json");
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                resp_body.len(),
                resp_body,
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    }
}

/// One JSON-RPC call's response object
fn answer(call: &serde_json::Value, fixtures: &HashMap<String, String>) -> serde_json::Value {
    let id = &call["id"];
    let block = (call["method"] == "getblock")
        .then(|| fixtures.get(call["params"][0].as_str().unwrap_or("")))
        .flatten();
    match block {
        Some(hex) => {
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": hex, "error": null })
        }
        None => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": null,
            "error": {"code": -8, "message": "Block not found"},
        }),
    }
}

type Blocks = Pin<Box<dyn Stream<Item = Result<BlockAndHash, Status>> + Send>>;

/// zebrad's indexer over the fixtures: `GetBlock` by hash, `GetBlockRange` by height (each range
/// recorded); a height past the fixtures = `NOT_FOUND` mid-stream, as zebrad ends a range at its
/// tip
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

/// Each fixture through `client` = the explorer's facts; by height = by hash
async fn assert_explorer_parity<S: ChainDataSource>(client: &S, label: &str) {
    for expected in FIXTURES {
        let height = BlockHeight::from(expected.height);
        let hash: BlockHash = expected.hash.parse().expect("display hex");
        let block = client
            .get_block_by_hash(hash)
            .await
            .unwrap_or_else(|e| panic!("{label}: block {} failed: {e}", expected.hash));
        assert_eq!(block.header().height, height, "{label}: coinbase height");
        let by_height = client
            .get_blocks_by_height(&[height])
            .await
            .unwrap_or_else(|e| panic!("{label}: block at {height} failed: {e}"))
            .remove(0);
        let txids = |block: &zaino_primitives::types::Block| {
            block.transactions().iter().map(|tx| tx.txid).collect::<Vec<_>>()
        };
        assert_eq!(
            (by_height.header(), txids(&by_height)),
            (block.header(), txids(&block)),
            "{label}: by height = by hash at {height}"
        );

        let header = block.header();
        let (hash, prev_hash) = (header.hash.to_string(), header.prev_hash.to_string());
        let got = (hash.as_str(), prev_hash.as_str(), header.time, block.transactions().len());
        let want = (expected.hash, expected.prev_hash, expected.time, expected.tx_count);
        assert_eq!(got, want, "{label}: (hash, prev_hash, time, tx count) at {height}");

        let mut t_in = 0usize;
        let mut t_out = 0usize;
        let mut s_spends = 0usize;
        let mut s_outputs = 0usize;
        let mut o_actions = 0usize;
        for tx in block.transactions() {
            t_in += tx.transparent.inputs.len();
            t_out += tx.transparent.outputs.len();
            s_spends += tx.sapling.spends.len();
            s_outputs += tx.sapling.outputs.len();
            o_actions += tx.orchard.actions.len();
        }

        let got = (t_in, t_out, s_spends, s_outputs, o_actions);
        let want = (
            expected.transparent_inputs,
            expected.transparent_outputs,
            expected.sapling_spends,
            expected.sapling_outputs,
            expected.orchard_actions,
        );
        assert_eq!(got, want, "{label}: (t in, t out, s spends, s outputs, o actions) at {height}");
    }
}

/// - Both clients: every fixture = the explorer's facts (one decoder, two transports)
/// - gRPC: heights cut into one `GetBlockRange` per consecutive run, blocks in asked order; a
///   height past the indexer's tip = `HeightNotFound` for it; undecodable bytes = `Parse`
#[tokio::test]
async fn both_clients_decode_every_block_to_the_explorers_facts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
    let rpc_address = listener.local_addr().expect("local addr").to_string();
    tokio::spawn(mock_zebra_rpc(listener, fixture_hex()));

    let mut by_height: HashMap<u32, Vec<u8>> =
        FIXTURES.iter().map(|f| (f.height, block(f.height))).collect();
    let by_hash = FIXTURES
        .iter()
        .map(|f| {
            let display: [u8; 32] = const_hex::decode_to_array(f.hash).expect("display hex");
            (display, block(f.height))
        })
        .collect();
    // three consecutive heights + a corrupt one, for the range cases below
    let first = block(FIXTURES[0].height);
    by_height.extend([(10, first.clone()), (11, first.clone()), (12, first), (30, vec![0xff; 8])]);
    let indexer = Arc::new(FakeIndexer { by_height, by_hash, ranges: Mutex::default() });
    let socket = TcpListener::bind("127.0.0.1:0").await.expect("bind fake indexer");
    let indexer_address = socket.local_addr().expect("local addr").to_string();
    let server = tonic::transport::Server::builder()
        .add_service(IndexerServer::from_arc(Arc::clone(&indexer)))
        .serve_with_incoming(tonic::transport::server::TcpIncoming::from(socket));
    tokio::spawn(server);

    let (timeouts, limits) = (Timeouts::default(), LinkLimits::default());
    let legacy = || ZcashLegacyRpcClient::at(&rpc_address, None, None, None, timeouts, limits);
    let legacy_client = legacy().expect("host:port");
    let grpc =
        ZcashGrpcClient::at(legacy().expect("host:port"), &indexer_address, None, timeouts, limits)
            .expect("host:port");
    assert_explorer_parity(&legacy_client, "legacy").await;
    assert_explorer_parity(&grpc, "grpc").await;

    let h = BlockHeight::from;
    indexer.ranges.lock().expect("ranges").clear();
    let blocks = grpc.get_blocks_by_height(&[h(10), h(11), h(12), h(419_200)]).await;
    let heights: Vec<_> =
        blocks.expect("all served").iter().map(|block| u32::from(block.header().height)).collect();
    assert_eq!(heights, [419_200; 4], "every block, asked order (all four = the first fixture)");
    let ranges = indexer.ranges.lock().expect("ranges").clone();
    assert_eq!(ranges, [(10, 12), (419_200, 419_200)], "one range per consecutive run");

    let past_tip = grpc.get_blocks_by_height(&[h(12), h(13)]).await;
    assert!(
        matches!(past_tip, Err(QueryError::Domain(GetAtHeightError::HeightNotFound(at))) if at == h(13)),
        "{past_tip:?}"
    );
    let corrupt = grpc.get_blocks_by_height(&[h(30)]).await;
    assert!(
        matches!(&corrupt, Err(QueryError::NonDomain(error)) if error.mode == FailureMode::Parse),
        "{corrupt:?}"
    );
}
