# `zaino-source-zebra` — usage

The composite. Holds both Zebra adapters and implements every `zaino-source`
port by routing each question to whichever transport can answer it.

```rust
use zaino_source_zebra::ZebraValidator;

// RPC only — Zaino and Zebra on different hosts
let validator = ZebraValidator::rpc_only(addr, auth, network)?;

// RPC + read-state — same host, sharing the database
let validator = ZebraValidator::spawn_direct(config).await?;
```

```rust
pub struct ZebraValidator {
    rpc: ZebraRpcAdapter,                        // always present
    readstate: Option<ZebraReadStateAdapter>,    // optional accelerator
}
```

The shape is the design. The old `Fetch` / `State` enum treated the two as
alternatives and matched on them in every method to answer two unrelated
questions at once — *can* this transport serve the query, and *should* it. Here
capability is structural (an adapter that cannot answer a question does not
implement its port) and preference is a routing table (this file).

`Fetch`-only and `State` are now **configurations of one type**, not variants.
The read-state path is never the only path.

## The three routing rules

```rust
fast_or_slow!(self, method, args)    // read-state where available, RPC otherwise
fast_then_slow!(self, method, args)  // try read-state, fall back to RPC on a retryable miss
self.rpc.method(args)                // RPC only — the state service cannot answer
```

- **`fast_or_slow`** is the default for anything both can answer.
- **`fast_then_slow`** is for questions where the read-state answer can be
  *incomplete* rather than wrong. `GetBlockByHash` is the canonical case: a
  side-chain block is not in the finalised state, and the RPC path can still
  find it. This preserves a behaviour the old connector had as an undocumented
  per-variant difference.
- **RPC only** for the mempool and the passthrough RPCs.

## Two places the usual preference inverts

Both are zcashd-era methods that **zebrad does not implement**, so the
read-state derivation is the only implementation that exists:

- `getblockdeltas` — routed to read-state first.
- `getaddressdeltas` — same.

Against zebrad the RPC fallback answers `-32601 Method not found`, so on an
RPC-only deployment these methods have no answer at all. That is a real
deployment consequence, not a detail: a Zaino running remotely from its Zebra
cannot serve them.

## Adding a port

1. Check what each transport can actually answer — including whether zebrad
   implements the RPC at all. Do not assume; `getblockdeltas` was wrongly
   omitted from the read-state adapter on exactly that assumption.
2. Pick a rule above. If none fits, say why in a comment on the impl rather
   than inventing a fourth macro.
3. If the routing choice is not obvious from the rule, write the reason at the
   call site. Every inverted preference in this file has one.

## Related

- ADR-0008 — capability vs preference, and why the enum went.
- `zaino-source-zebra-readstate` — what the fast path can and cannot answer.
