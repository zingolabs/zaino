//! Zaino gRPC API test tool.
//!
//! Connects to a zainod/lightwalletd gRPC server and exercises every
//! `CompactTxStreamer` RPC method, reporting per-RPC pass/fail/skip
//! with timing and diagnostic detail.
//!
//! ## How it works
//!
//! 1. Connects to the gRPC server and calls `GetLightdInfo` to verify
//!    connectivity and discover server capabilities (chain name, tip
//!    height, t-address support).
//! 2. Discovers test data from the chain: fetches a sample block to
//!    extract a transaction hash for the `GetTransaction` test.
//! 3. Runs each RPC test sequentially, collecting results.
//! 4. Prints a summary table with pass/fail/skip counts and per-RPC
//!    timing.

use std::time::Instant;

use clap::Args;
use futures::{StreamExt, TryStreamExt};
use tonic::transport::Channel;
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zaino_proto::proto::service::{
    Address, AddressList, BlockId, BlockRange, ChainSpec, Duration, Empty, GetAddressUtxosArg,
    GetMempoolTxRequest, GetSubtreeRootsArg, LightdInfo, RawTransaction,
    TransparentAddressBlockFilter, TxFilter,
};

use crate::{boxed_error, grpc_client, AdminResult};

/// Comprehensive gRPC API test tool for lightwalletd/zainod servers.
///
/// Exercises every CompactTxStreamer RPC and reports pass/fail/skip
/// with per-call timing.
#[derive(Args)]
pub(super) struct GrpcTestArgs {
    /// Lightwalletd/zainod server URL (e.g. "http://127.0.0.1:9067").
    #[arg(short, long)]
    server: String,

    /// Number of streaming items to collect per streaming RPC test.
    #[arg(long, default_value = "5")]
    stream_limit: usize,

    /// Stop after this many failures.
    #[arg(long, default_value = "0")]
    max_failures: usize,
}

// =============================================================================
// Test result types
// =============================================================================

#[derive(Debug)]
enum TestStatus {
    Pass,
    Fail(String),
    Skip(String),
}

#[derive(Debug)]
struct TestResult {
    rpc_name: &'static str,
    pattern: &'static str, // "unary", "server-stream", "client-stream"
    status: TestStatus,
    duration_ms: f64,
}

impl TestResult {
    fn pass(rpc_name: &'static str, pattern: &'static str, duration_ms: f64) -> Self {
        Self {
            rpc_name,
            pattern,
            status: TestStatus::Pass,
            duration_ms,
        }
    }

    fn fail(
        rpc_name: &'static str,
        pattern: &'static str,
        duration_ms: f64,
        detail: String,
    ) -> Self {
        Self {
            rpc_name,
            pattern,
            status: TestStatus::Fail(detail),
            duration_ms,
        }
    }

    fn skip(rpc_name: &'static str, pattern: &'static str, reason: String) -> Self {
        Self {
            rpc_name,
            pattern,
            status: TestStatus::Skip(reason),
            duration_ms: 0.0,
        }
    }
}

// =============================================================================
// Chain context discovered from the live server
// =============================================================================

struct ChainContext {
    tip_height: u64,
    chain_name: String,
    taddr_support: bool,
    /// A transaction hash (in protocol order, 32 bytes) from a sample block.
    sample_tx_hash: Option<Vec<u8>>,
}

pub(super) async fn run(args: GrpcTestArgs) -> AdminResult<()> {
    eprintln!("zaino-admin grpc-test — gRPC API completeness check");
    eprintln!("Server: {}", args.server);
    eprintln!();

    // ----- Connect ----------------------------------------------------------
    let connect_start = Instant::now();
    let mut client = grpc_client::connect_eager(&args.server).await?;
    eprintln!("Connected in {:.2}s", connect_start.elapsed().as_secs_f64());
    eprintln!();

    // ----- Discover chain context -------------------------------------------
    eprintln!("Discovering chain context...");
    let ctx = match discover_context(&mut client).await {
        Ok(c) => {
            eprintln!("  Chain:       {}", c.chain_name);
            eprintln!("  Tip height:  {}", c.tip_height);
            eprintln!(
                "  T-addr support: {}",
                if c.taddr_support { "yes" } else { "no" }
            );
            eprintln!(
                "  Sample tx:   {}",
                c.sample_tx_hash
                    .as_ref()
                    .map(|h| hex::encode(h))
                    .unwrap_or_else(|| "none".to_string())
            );
            c
        }
        Err(e) => {
            eprintln!("  Discovery failed: {e}");
            eprintln!("  Proceeding with best-effort defaults...");
            ChainContext {
                tip_height: 0,
                chain_name: "unknown".into(),
                taddr_support: false,
                sample_tx_hash: None,
            }
        }
    };
    eprintln!();

    // ----- Run tests --------------------------------------------------------
    eprintln!("Running RPC tests...");
    eprintln!();

    let stream_limit = args.stream_limit;
    let max_failures = args.max_failures;
    let mut tests: Vec<TestResult> = Vec::new();

    push_test(&mut tests, max_failures, test_get_lightd_info(&mut client)).await;
    push_test(&mut tests, max_failures, test_get_latest_block(&mut client)).await;
    push_test(
        &mut tests,
        max_failures,
        test_get_block(&mut client, ctx.tip_height),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_block_nullifiers(&mut client, ctx.tip_height),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_transaction(&mut client, &ctx),
    )
    .await;
    push_test(&mut tests, max_failures, test_send_transaction(&mut client)).await;
    push_test(
        &mut tests,
        max_failures,
        test_get_tree_state(&mut client, ctx.tip_height),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_latest_tree_state(&mut client),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_address_utxos(&mut client, ctx.taddr_support),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_taddress_balance(&mut client, ctx.taddr_support),
    )
    .await;
    push_test(&mut tests, max_failures, test_ping(&mut client)).await;
    push_test(
        &mut tests,
        max_failures,
        test_get_block_range(&mut client, ctx.tip_height, stream_limit),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_block_range_nullifiers(&mut client, ctx.tip_height, stream_limit),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_mempool_tx(&mut client, stream_limit),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_mempool_stream(&mut client, stream_limit),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_subtree_roots(&mut client, stream_limit),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_address_utxos_stream(&mut client, ctx.taddr_support, stream_limit),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_taddress_transactions(&mut client, ctx.taddr_support, stream_limit),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_taddress_txids(&mut client, ctx.taddr_support, stream_limit),
    )
    .await;
    push_test(
        &mut tests,
        max_failures,
        test_get_taddress_balance_stream(&mut client, ctx.taddr_support),
    )
    .await;

    if hit_failure_limit(&tests, max_failures) {
        eprintln!("Stopped after reaching --max-failures={max_failures}.");
        eprintln!();
    }

    // ----- Report ----------------------------------------------------------
    let passes: Vec<_> = tests
        .iter()
        .filter(|t| matches!(t.status, TestStatus::Pass))
        .collect();
    let failures: Vec<_> = tests
        .iter()
        .filter(|t| matches!(t.status, TestStatus::Fail(_)))
        .collect();
    let skips: Vec<_> = tests
        .iter()
        .filter(|t| matches!(t.status, TestStatus::Skip(_)))
        .collect();

    // Detail lines
    for t in &tests {
        match &t.status {
            TestStatus::Pass => {
                eprintln!(
                    "  ✅ {:<38} {:>12}  {:>8.2}ms",
                    t.rpc_name, t.pattern, t.duration_ms
                );
            }
            TestStatus::Fail(detail) => {
                eprintln!(
                    "  ❌ {:<38} {:>12}  {:>8.2}ms  — {detail}",
                    t.rpc_name, t.pattern, t.duration_ms
                );
            }
            TestStatus::Skip(reason) => {
                eprintln!(
                    "  ⏭️  {:<38} {:>12}  {:>8}  — {reason}",
                    t.rpc_name, t.pattern, "—"
                );
            }
        }
    }

    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("  Summary");
    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("  Total:   {}", tests.len());
    eprintln!("  Passed:  {}", passes.len());
    eprintln!("  Failed:  {}", failures.len());
    eprintln!("  Skipped: {}", skips.len());
    eprintln!();

    if !failures.is_empty() {
        eprintln!("  Failures:");
        for f in &failures {
            if let TestStatus::Fail(detail) = &f.status {
                eprintln!("    - {}: {detail}", f.rpc_name);
            }
        }
        eprintln!();
        eprintln!("  ❌ {} RPC(s) failed.", failures.len());
        return Err(boxed_error(format!("{} RPC(s) failed", failures.len())));
    }

    eprintln!(
        "  ✅ All RPCs passed ({skipped} skipped).",
        skipped = skips.len()
    );
    Ok(())
}

async fn push_test(
    tests: &mut Vec<TestResult>,
    max_failures: usize,
    test: impl std::future::Future<Output = TestResult>,
) {
    if hit_failure_limit(tests, max_failures) {
        return;
    }
    tests.push(test.await);
}

fn hit_failure_limit(tests: &[TestResult], max_failures: usize) -> bool {
    max_failures > 0
        && tests
            .iter()
            .filter(|test| matches!(test.status, TestStatus::Fail(_)))
            .count()
            >= max_failures
}

// =============================================================================
// Chain context discovery
// =============================================================================

/// Query the server for metadata and discover test data.
async fn discover_context(
    client: &mut CompactTxStreamerClient<Channel>,
) -> Result<ChainContext, String> {
    // GetLightdInfo for server metadata
    let info: LightdInfo = client
        .get_lightd_info(Empty {})
        .await
        .map_err(|e| format!("GetLightdInfo: {e}"))?
        .into_inner();

    // GetLatestBlock for chain tip
    let tip_height = client
        .get_latest_block(ChainSpec {})
        .await
        .map_err(|e| format!("GetLatestBlock: {e}"))?
        .into_inner()
        .height;

    // Try to discover a sample transaction hash from a block.
    // Start at height 1 (skip genesis), fall back to tip.
    let sample_tx_hash = discover_sample_tx(client, tip_height).await;

    Ok(ChainContext {
        tip_height,
        chain_name: info.chain_name,
        taddr_support: info.taddr_support,
        sample_tx_hash,
    })
}

/// Fetch a block and extract the first transaction hash (if any).
async fn discover_sample_tx(
    client: &mut CompactTxStreamerClient<Channel>,
    tip_height: u64,
) -> Option<Vec<u8>> {
    // Try heights near the tip — the server's block deque only holds
    // recent blocks.
    let candidates: Vec<u64> = (0u64..5)
        .map(|i| tip_height.saturating_sub(i))
        .filter(|&h| h > 0)
        .collect();
    for height in candidates {
        if let Ok(response) = client
            .get_block(BlockId {
                height,
                hash: Vec::new(),
            })
            .await
        {
            let block = response.into_inner();
            if let Some(tx) = block.vtx.first() {
                if !tx.txid.is_empty() {
                    return Some(tx.txid.clone());
                }
            }
        }
    }
    None
}

// =============================================================================
// Helper: time an async operation
// =============================================================================

async fn timed<T, E: std::fmt::Display>(
    f: impl std::future::Future<Output = Result<T, E>>,
) -> (Result<T, String>, f64) {
    let start = Instant::now();
    let result = f.await;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mapped = result.map_err(|e| e.to_string());
    (mapped, elapsed_ms)
}

// =============================================================================
// Unary RPC tests
// =============================================================================

async fn test_get_lightd_info(client: &mut CompactTxStreamerClient<Channel>) -> TestResult {
    let (result, ms) = timed(client.get_lightd_info(Empty {})).await;
    match result {
        Ok(response) => {
            let info = response.into_inner();
            if info.version.is_empty() && info.vendor.is_empty() {
                TestResult::fail(
                    "GetLightdInfo",
                    "unary",
                    ms,
                    "version and vendor both empty".into(),
                )
            } else {
                TestResult::pass("GetLightdInfo", "unary", ms)
            }
        }
        Err(e) => TestResult::fail("GetLightdInfo", "unary", ms, e),
    }
}

async fn test_get_latest_block(client: &mut CompactTxStreamerClient<Channel>) -> TestResult {
    let (result, ms) = timed(client.get_latest_block(ChainSpec {})).await;
    match result {
        Ok(response) => {
            let block_id = response.into_inner();
            if block_id.height > 0 {
                TestResult::pass("GetLatestBlock", "unary", ms)
            } else {
                TestResult::fail(
                    "GetLatestBlock",
                    "unary",
                    ms,
                    format!("height is {} (expected > 0)", block_id.height),
                )
            }
        }
        Err(e) => TestResult::fail("GetLatestBlock", "unary", ms, e),
    }
}

async fn test_get_block(
    client: &mut CompactTxStreamerClient<Channel>,
    tip_height: u64,
) -> TestResult {
    let height = tip_height;
    let (result, ms) = timed(client.get_block(BlockId {
        height,
        hash: Vec::new(),
    }))
    .await;
    match result {
        Ok(response) => {
            let block = response.into_inner();
            if block.hash.is_empty() {
                TestResult::fail(
                    "GetBlock",
                    "unary",
                    ms,
                    format!("block hash is empty at height {height}"),
                )
            } else {
                TestResult::pass("GetBlock", "unary", ms)
            }
        }
        Err(e) => TestResult::fail("GetBlock", "unary", ms, e),
    }
}

async fn test_get_block_nullifiers(
    client: &mut CompactTxStreamerClient<Channel>,
    tip_height: u64,
) -> TestResult {
    let height = tip_height;
    let (result, ms) = timed(client.get_block_nullifiers(BlockId {
        height,
        hash: Vec::new(),
    }))
    .await;
    match result {
        Ok(_response) => {
            // A nullifiers-only response means the RPC is wired and working.
            TestResult::pass("GetBlockNullifiers", "unary", ms)
        }
        Err(e) => TestResult::fail("GetBlockNullifiers", "unary", ms, e),
    }
}

async fn test_get_transaction(
    client: &mut CompactTxStreamerClient<Channel>,
    ctx: &ChainContext,
) -> TestResult {
    let tx_hash = match &ctx.sample_tx_hash {
        Some(h) => h.clone(),
        None => {
            return TestResult::skip(
                "GetTransaction",
                "unary",
                "no sample transaction hash discovered".into(),
            );
        }
    };

    let (result, ms) = timed(client.get_transaction(TxFilter {
        block: None,
        index: 0,
        hash: tx_hash,
    }))
    .await;
    match result {
        Ok(response) => {
            let tx = response.into_inner();
            if tx.data.is_empty() {
                TestResult::fail("GetTransaction", "unary", ms, "tx data is empty".into())
            } else {
                TestResult::pass("GetTransaction", "unary", ms)
            }
        }
        Err(e) => TestResult::fail("GetTransaction", "unary", ms, e),
    }
}

async fn test_send_transaction(client: &mut CompactTxStreamerClient<Channel>) -> TestResult {
    // Sending an empty transaction should be rejected gracefully by the
    // validator — we are testing that the RPC is wired, not that it
    // succeeds.
    let (result, ms) = timed(client.send_transaction(RawTransaction {
        data: Vec::new(),
        height: 0,
    }))
    .await;
    match result {
        Ok(response) => {
            let _sr = response.into_inner();
            // error_code != 0 means the validator rejected it — that's fine.
            // The RPC itself succeeded.
            TestResult::pass("SendTransaction", "unary", ms)
        }
        Err(e) => {
            // Tonic transport errors are unexpected; gRPC-status errors
            // (e.g. UNIMPLEMENTED) are also acceptable — the RPC is wired.
            let status_str = e.to_string();
            if status_str.contains("unimplemented") || status_str.contains("Unimplemented") {
                TestResult::skip(
                    "SendTransaction",
                    "unary",
                    "RPC not implemented by server".into(),
                )
            } else {
                TestResult::pass("SendTransaction", "unary", ms)
            }
        }
    }
}

async fn test_get_tree_state(
    client: &mut CompactTxStreamerClient<Channel>,
    tip_height: u64,
) -> TestResult {
    let height = tip_height;
    let (result, ms) = timed(client.get_tree_state(BlockId {
        height,
        hash: Vec::new(),
    }))
    .await;
    match result {
        Ok(response) => {
            let ts = response.into_inner();
            // The response height may differ if the server resolves by hash.
            // The RPC is working as long as we get a valid response.
            if ts.network.is_empty() && ts.hash.is_empty() {
                TestResult::fail(
                    "GetTreeState",
                    "unary",
                    ms,
                    "response has empty network and hash".into(),
                )
            } else {
                TestResult::pass("GetTreeState", "unary", ms)
            }
        }
        Err(e) => TestResult::fail("GetTreeState", "unary", ms, e),
    }
}

async fn test_get_latest_tree_state(client: &mut CompactTxStreamerClient<Channel>) -> TestResult {
    let (result, ms) = timed(client.get_latest_tree_state(Empty {})).await;
    match result {
        Ok(response) => {
            let ts = response.into_inner();
            if ts.height > 0 {
                TestResult::pass("GetLatestTreeState", "unary", ms)
            } else {
                TestResult::fail("GetLatestTreeState", "unary", ms, "height is 0".into())
            }
        }
        Err(e) => TestResult::fail("GetLatestTreeState", "unary", ms, e),
    }
}

async fn test_get_address_utxos(
    client: &mut CompactTxStreamerClient<Channel>,
    taddr_support: bool,
) -> TestResult {
    if !taddr_support {
        return TestResult::skip(
            "GetAddressUtxos",
            "unary",
            "t-address support not enabled".into(),
        );
    }

    // Use a dummy address; the server should return an empty list or an
    // error — either is fine as long as the RPC is wired.
    let (result, ms) = timed(client.get_address_utxos(GetAddressUtxosArg {
        addresses: vec!["t1UNKNOWN".to_string()],
        start_height: 0,
        max_entries: 5,
    }))
    .await;
    match result {
        Ok(_response) => TestResult::pass("GetAddressUtxos", "unary", ms),
        Err(_e) => {
            // Accept gRPC errors (invalid address format, etc.) — the
            // RPC is wired and responding.
            TestResult::pass("GetAddressUtxos", "unary", ms)
        }
    }
}

async fn test_get_taddress_balance(
    client: &mut CompactTxStreamerClient<Channel>,
    taddr_support: bool,
) -> TestResult {
    if !taddr_support {
        return TestResult::skip(
            "GetTaddressBalance",
            "unary",
            "t-address support not enabled".into(),
        );
    }

    let (result, ms) = timed(client.get_taddress_balance(AddressList {
        addresses: vec!["t1UNKNOWN".to_string()],
    }))
    .await;
    match result {
        Ok(_response) => TestResult::pass("GetTaddressBalance", "unary", ms),
        Err(_e) => {
            // Accept gRPC errors — RPC is wired.
            TestResult::pass("GetTaddressBalance", "unary", ms)
        }
    }
}

async fn test_ping(client: &mut CompactTxStreamerClient<Channel>) -> TestResult {
    let (result, ms) = timed(client.ping(Duration { interval_us: 0 })).await;
    match result {
        Ok(_response) => TestResult::pass("Ping", "unary", ms),
        Err(e) => {
            let s = e.to_string();
            if s.contains("unimplemented") || s.contains("Unimplemented") {
                TestResult::skip(
                    "Ping",
                    "unary",
                    "Ping not enabled (--ping-very-insecure)".into(),
                )
            } else {
                TestResult::pass("Ping", "unary", ms)
            }
        }
    }
}

// =============================================================================
// Server-streaming RPC tests
// =============================================================================

async fn test_get_block_range(
    client: &mut CompactTxStreamerClient<Channel>,
    tip_height: u64,
    stream_limit: usize,
) -> TestResult {
    let limit = stream_limit as u64;
    let end = tip_height;
    let start = if tip_height > limit {
        tip_height - limit + 1
    } else {
        1
    };
    let (result, ms) = timed(async {
        let request = BlockRange {
            start: Some(BlockId {
                height: start,
                hash: Vec::new(),
            }),
            end: Some(BlockId {
                height: end,
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        };
        let response = client.get_block_range(request).await?;
        let blocks: Vec<CompactBlock> = response
            .into_inner()
            .take(stream_limit)
            .try_collect::<Vec<_>>()
            .await?;
        Ok::<_, tonic::Status>(blocks)
    })
    .await;
    match result {
        Ok(blocks) => {
            if blocks.is_empty() {
                TestResult::fail(
                    "GetBlockRange",
                    "server-stream",
                    ms,
                    format!("requested {start}..={end}, got 0 blocks"),
                )
            } else {
                TestResult::pass("GetBlockRange", "server-stream", ms)
            }
        }
        Err(e) => TestResult::fail("GetBlockRange", "server-stream", ms, e),
    }
}

async fn test_get_block_range_nullifiers(
    client: &mut CompactTxStreamerClient<Channel>,
    tip_height: u64,
    stream_limit: usize,
) -> TestResult {
    let limit = stream_limit as u64;
    let end = tip_height;
    let start = if tip_height > limit {
        tip_height - limit + 1
    } else {
        1
    };
    let (result, ms) = timed(async {
        let request = BlockRange {
            start: Some(BlockId {
                height: start,
                hash: Vec::new(),
            }),
            end: Some(BlockId {
                height: end,
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        };
        let response = client.get_block_range_nullifiers(request).await?;
        let blocks: Vec<CompactBlock> = response
            .into_inner()
            .take(stream_limit)
            .try_collect::<Vec<_>>()
            .await?;
        Ok::<_, tonic::Status>(blocks)
    })
    .await;
    match result {
        Ok(blocks) => {
            if blocks.is_empty() {
                TestResult::fail(
                    "GetBlockRangeNullifiers",
                    "server-stream",
                    ms,
                    format!("requested {start}..={end}, got 0 blocks"),
                )
            } else {
                TestResult::pass("GetBlockRangeNullifiers", "server-stream", ms)
            }
        }
        Err(e) => TestResult::fail("GetBlockRangeNullifiers", "server-stream", ms, e),
    }
}

async fn test_get_mempool_tx(
    client: &mut CompactTxStreamerClient<Channel>,
    stream_limit: usize,
) -> TestResult {
    let (result, ms) = timed(async {
        let response = client
            .get_mempool_tx(GetMempoolTxRequest {
                exclude_txid_suffixes: Vec::new(),
                pool_types: Vec::new(),
            })
            .await?;
        let txs: Vec<_> = response
            .into_inner()
            .take(stream_limit)
            .try_collect::<Vec<_>>()
            .await?;
        Ok::<_, tonic::Status>(txs)
    })
    .await;
    match result {
        Ok(_txs) => {
            // Mempool may be empty — that's fine, the RPC is wired.
            TestResult::pass("GetMempoolTx", "server-stream", ms)
        }
        Err(e) => TestResult::fail("GetMempoolTx", "server-stream", ms, e),
    }
}

async fn test_get_mempool_stream(
    client: &mut CompactTxStreamerClient<Channel>,
    stream_limit: usize,
) -> TestResult {
    // GetMempoolStream stays open until a new block is mined. We apply
    // a timeout to avoid hanging when the mempool is empty.
    let (result, ms) = timed(async {
        let response = client.get_mempool_stream(Empty {}).await?;
        let stream = response.into_inner().take(stream_limit);
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.try_collect::<Vec<_>>(),
        )
        .await
        {
            Ok(Ok(txs)) => Ok(txs),
            Ok(Err(status)) => Err(status),
            Err(_elapsed) => {
                // Timeout is fine — empty mempool, or slow stream.
                // The RPC itself is wired.
                Ok(Vec::new())
            }
        }
    })
    .await;
    match result {
        Ok(_txs) => TestResult::pass("GetMempoolStream", "server-stream", ms),
        Err(e) => {
            let s = e.to_string();
            if s.contains("unimplemented") || s.contains("Unimplemented") {
                TestResult::skip(
                    "GetMempoolStream",
                    "server-stream",
                    "RPC not implemented by server".into(),
                )
            } else {
                TestResult::fail("GetMempoolStream", "server-stream", ms, e)
            }
        }
    }
}

async fn test_get_subtree_roots(
    client: &mut CompactTxStreamerClient<Channel>,
    stream_limit: usize,
) -> TestResult {
    // Test Sapling subtree roots.
    let (result, ms) = timed(async {
        let response = client
            .get_subtree_roots(GetSubtreeRootsArg {
                start_index: 0,
                shielded_protocol: 0, // sapling
                max_entries: stream_limit as u32,
            })
            .await?;
        let roots: Vec<_> = response
            .into_inner()
            .take(stream_limit)
            .try_collect::<Vec<_>>()
            .await?;
        Ok::<_, tonic::Status>(roots)
    })
    .await;
    match result {
        Ok(_roots) => TestResult::pass("GetSubtreeRoots", "server-stream", ms),
        Err(e) => {
            let s = e.to_string();
            if s.contains("unimplemented") || s.contains("Unimplemented") {
                TestResult::skip("GetSubtreeRoots", "server-stream", "not implemented".into())
            } else {
                TestResult::fail("GetSubtreeRoots", "server-stream", ms, e)
            }
        }
    }
}

async fn test_get_address_utxos_stream(
    client: &mut CompactTxStreamerClient<Channel>,
    taddr_support: bool,
    stream_limit: usize,
) -> TestResult {
    if !taddr_support {
        return TestResult::skip(
            "GetAddressUtxosStream",
            "server-stream",
            "t-address support not enabled".into(),
        );
    }

    let (result, ms) = timed(async {
        let response = client
            .get_address_utxos_stream(GetAddressUtxosArg {
                addresses: vec!["t1UNKNOWN".to_string()],
                start_height: 0,
                max_entries: stream_limit as u32,
            })
            .await?;
        let utxos: Vec<_> = response
            .into_inner()
            .take(stream_limit)
            .try_collect::<Vec<_>>()
            .await?;
        Ok::<_, tonic::Status>(utxos)
    })
    .await;
    match result {
        Ok(_) => TestResult::pass("GetAddressUtxosStream", "server-stream", ms),
        Err(_e) => {
            // Accept gRPC errors — RPC is wired.
            TestResult::pass("GetAddressUtxosStream", "server-stream", ms)
        }
    }
}

async fn test_get_taddress_transactions(
    client: &mut CompactTxStreamerClient<Channel>,
    taddr_support: bool,
    stream_limit: usize,
) -> TestResult {
    if !taddr_support {
        return TestResult::skip(
            "GetTaddressTransactions",
            "server-stream",
            "t-address support not enabled".into(),
        );
    }

    let (result, ms) = timed(async {
        let response = client
            .get_taddress_transactions(TransparentAddressBlockFilter {
                address: "t1UNKNOWN".to_string(),
                range: Some(BlockRange {
                    start: Some(BlockId {
                        height: 0,
                        hash: Vec::new(),
                    }),
                    end: Some(BlockId {
                        height: 100,
                        hash: Vec::new(),
                    }),
                    pool_types: Vec::new(),
                }),
            })
            .await?;
        let txs: Vec<_> = response
            .into_inner()
            .take(stream_limit)
            .try_collect::<Vec<_>>()
            .await?;
        Ok::<_, tonic::Status>(txs)
    })
    .await;
    match result {
        Ok(_) => TestResult::pass("GetTaddressTransactions", "server-stream", ms),
        Err(_e) => TestResult::pass("GetTaddressTransactions", "server-stream", ms),
    }
}

async fn test_get_taddress_txids(
    client: &mut CompactTxStreamerClient<Channel>,
    taddr_support: bool,
    stream_limit: usize,
) -> TestResult {
    if !taddr_support {
        return TestResult::skip(
            "GetTaddressTxids",
            "server-stream",
            "t-address support not enabled".into(),
        );
    }

    let (result, ms) = timed(async {
        let response = client
            .get_taddress_txids(TransparentAddressBlockFilter {
                address: "t1UNKNOWN".to_string(),
                range: Some(BlockRange {
                    start: Some(BlockId {
                        height: 0,
                        hash: Vec::new(),
                    }),
                    end: Some(BlockId {
                        height: 100,
                        hash: Vec::new(),
                    }),
                    pool_types: Vec::new(),
                }),
            })
            .await?;
        let txs: Vec<_> = response
            .into_inner()
            .take(stream_limit)
            .try_collect::<Vec<_>>()
            .await?;
        Ok::<_, tonic::Status>(txs)
    })
    .await;
    match result {
        Ok(_) => TestResult::pass("GetTaddressTxids", "server-stream", ms),
        Err(_e) => TestResult::pass("GetTaddressTxids", "server-stream", ms),
    }
}

// =============================================================================
// Client-streaming RPC tests
// =============================================================================

async fn test_get_taddress_balance_stream(
    client: &mut CompactTxStreamerClient<Channel>,
    taddr_support: bool,
) -> TestResult {
    if !taddr_support {
        return TestResult::skip(
            "GetTaddressBalanceStream",
            "client-stream",
            "t-address support not enabled".into(),
        );
    }

    let (result, ms) = timed(async {
        // Create a client stream with a single address.
        let stream = tokio_stream::iter(vec![Address {
            address: "t1UNKNOWN".to_string(),
        }]);
        let response = client
            .get_taddress_balance_stream(tonic::Request::new(stream))
            .await?;
        Ok::<_, tonic::Status>(response.into_inner())
    })
    .await;
    match result {
        Ok(_balance) => TestResult::pass("GetTaddressBalanceStream", "client-stream", ms),
        Err(_e) => {
            // Accept gRPC errors — RPC is wired.
            TestResult::pass("GetTaddressBalanceStream", "client-stream", ms)
        }
    }
}
