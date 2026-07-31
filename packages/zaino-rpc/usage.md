# `zaino-rpc` — usage

The JSON-RPC transport. HTTP, the request/response envelope, authentication,
and one retry rule. Nothing else.

## Scope, stated precisely

`call()` returns a raw `serde_json::Value`. **Response parsing is the adapter's
job**, not this crate's:

```rust
use zaino_rpc::{RpcClient, RpcClientConfig};

let client = RpcClient::new(config)?;
let value = client.call("getblockchaininfo", vec![]).await?;
// -> serde_json::Value; interpreting it is zaino-source-zebra-rpc's problem
```

This is the boundary that lets one client serve both the production adapter
(`zaino-source-zebra-rpc`, which parses into domain types) and the live tests'
`ValidatorOracle` (which compares raw JSON against Zaino's answers). An oracle
that shared the parser under test would not be an oracle.

It is also why replacing `zaino-fetch` took two crates and not one:
`zaino-fetch` did transport *and* parsing *and* server-side serialization in one
place, and each of the three has a different owner now.

## Errors

`RpcError` distinguishes the envelope's failure modes, and converts into
`zaino_source::FetchError` so the source layer's `FailureMode` classification
works end to end:

```rust
RpcError::Rpc { code, message }   // the server answered with an error object
RpcError::Http(status)            // transport-level status
RpcError::Transport(..)           // connection, timeout
RpcError::Auth                    // credentials rejected
```

The `code` on `RpcError::Rpc` is the thing a zcashd-compatible client keys on,
so it must survive to the served response. It does, via
`FailureMode::RpcError(i64)` and the downcast walk in `zaino-serve`.

## The one retry this crate does

`-1` (work queue full) is retried here, at the transport, because it is a
property of the connection rather than of the question. Everything else is left
to `zaino_source::Resilient`, which has the policy and the backoff. Do not add
retry rules here — the layer above cannot see or override them.

## Probing a validator at startup

```rust
use zaino_rpc::{auth_from_parts, probe_node};

let auth = auth_from_parts(user, password)?;
let info = probe_node(&addr, auth).await?;   // 6 attempts, 3s apart
```

`probe_node` **returns an error**. Its predecessor in `zaino-fetch` called
`std::process::exit(1)`, which made the startup path untestable and gave an
embedding process no say in its own shutdown.

## Metrics

`metric_names` holds the outbound RPC metric names, moved here from `zainod`
because this crate is what emits them. Registration (and the descriptions) stay
with the daemon.

## Related

- `zaino-source-zebra-rpc` — parses what this returns.
- ADR-0008 — where this sits in the crate graph.
