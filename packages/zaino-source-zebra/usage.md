# `zaino-source-zebra` — usage

The Zebra validator source. Wraps the JSON-RPC adapter and implements every
`zaino-source` port by delegating to it, adding a synthesised chain-tip
subscription on top.

```rust
use std::time::Duration;
use zaino_source_zebra::ZebraValidator;

// Reach the validator over JSON-RPC.
let validator = ZebraValidator::rpc_only(rpc_adapter);

// Add a tip subscription by polling a second handle to the same validator.
let validator = validator
    .with_tip_polling(tip_probe, Duration::from_secs(2))
    .await?;
```

```rust
pub struct ZebraValidator {
    rpc: ZebraRpcAdapter,        // the JSON-RPC handle every query routes through
    tip: Option<PolledChainTip>, // synthesised tip subscription, once polling is on
}
```

## Tip subscription

Neither of Zebra's transports pushes a native tip stream, so the source
synthesises one by polling. `with_tip_polling` is opt-in and fallible rather
than part of construction: seeding the subscription takes one live read, so
folding it into construction would make a handle impossible to build while the
validator is down — exactly when an indexer most wants to start and retry. The
poll task owns its own source handle for its lifetime, so the caller passes a
second handle to the same validator.

`subscribe_to_chain_tip` returns the poller's receiver, or `None` when polling
was never started. If a transport ever gains a native stream, the port prefers
it and the poller becomes the fallback.

## Deployment consequences

Every query is answered over JSON-RPC. Two legacy-era methods —
`getblockdeltas` and `getaddressdeltas` — are not implemented by zebrad and
answer `-32601 Method not found`; they are served only by a legacy full node.
`getspentinfo` reads a spent index zebrad does not expose and answers
`Unsupported`.

## Related

- `zaino-source-zebra-rpc` — the JSON-RPC adapter and its error classification.
