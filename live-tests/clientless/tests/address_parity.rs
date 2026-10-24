//! Address-index requests zebrad 7 refuses are refused identically by zainod on
//! both backends: same JSON-RPC code (`-5` TEX, `-8` too many addresses) and
//! message, whichever path answers.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use zaino_testutils::ZEBRAD_VERSION;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const TEX: &str = "textest1qyqszqgpqyqszqgpqyqszqgpqyqszqgpfcjgfy";
/// One past zebrad 7's per-request address limit.
const TOO_MANY: usize = 10_001;

/// The `{code, message}` of a JSON-RPC error reply.
async fn rpc_error(rpc: &JsonRpcClient, method: &'static str, params: Value) -> Result<Value> {
    let error = rpc
        .call_value(method, params)
        .await
        .err()
        .with_context(|| format!("{method} must be refused"))?;
    let text = format!("{:#}", anyhow::Error::from(error));
    let body = text
        .split_once("RPC error: ")
        .map(|(_, body)| body)
        .with_context(|| format!("{method}: no RPC error body in `{text}`"))?;
    let body: Value = serde_json::from_str(body)?;
    Ok(json!({ "code": body["code"], "message": body["message"] }))
}

#[ztest::qos::integration(footprint = "3c/6Gi")]
#[tokio::test(flavor = "multi_thread")]
async fn address_queries_refuse_what_zebrad_refuses() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let vol = env.shared_volume("zebra-db");
    let validator = env.add_validator(Validator::zebrad(ZEBRAD_VERSION).regtest().mount(&vol));
    let fetch = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::Fetch)
            .named("zaino-fetch"),
    );
    let state = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::State)
            .mount(&vol)
            .named("zaino-state"),
    );
    env.build().await?;

    let tip = validator.generate_blocks(1).await?;
    for indexer in [&fetch, &state] {
        indexer.wait_for_block_num(tip, READY).await?;
    }

    let too_many = vec![FILLER_ADDRESS; TOO_MANY];
    let requests = [
        ("getaddressbalance", json!([{ "addresses": [TEX] }]), -5),
        ("getaddressutxos", json!([{ "addresses": [TEX] }]), -5),
        (
            "getaddresstxids",
            json!([{ "addresses": [TEX], "start": 1, "end": 1 }]),
            -5,
        ),
        ("getaddressbalance", json!([{ "addresses": too_many }]), -8),
        ("getaddressutxos", json!([{ "addresses": too_many }]), -8),
        (
            "getaddresstxids",
            json!([{ "addresses": too_many, "start": 1, "end": 1 }]),
            -8,
        ),
    ];

    let vrpc = validator.json_rpc().await?;
    for (method, params, code) in requests {
        let expected = rpc_error(&vrpc, method, params.clone()).await?;
        assert_eq!(expected["code"], code, "zebrad {method}: {expected}");
        for indexer in [&fetch, &state] {
            let served = rpc_error(&indexer.json_rpc().await?, method, params.clone()).await?;
            assert_eq!(served, expected, "{method}");
        }
    }
    Ok(())
}
