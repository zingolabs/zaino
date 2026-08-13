# `zaino-primitives` — usage

Zaino's domain vocabulary: the types that describe the Zcash chain in Zaino's
own terms, independent of how any of it is transported or stored.

## The one rule

**This crate's entire dependency list is `thiserror`.** That is not an
accident of the current implementation — it is the property that makes every
other crate able to depend on it. Adding a dependency here adds it to
`zaino-source`, both adapters, `zaino-state`, `zaino-serve` and `zainod` at
once.

In particular there is **no serde**. A serde derive in this crate would let the
wire format and the domain model start deciding each other, which is exactly
what ADR-0009 exists to prevent. Serialization lives at the boundary that owns
the format:

| direction | who owns the format |
|---|---|
| validator reply → domain | `zaino-source-zebra-rpc/src/parse.rs` |
| domain → served JSON | `zaino-serve/src/rpc/jsonrpc/wire/` |
| domain → disk | `zaino-state`'s `Persistent*` types |
| domain → gRPC | `zaino-proto`, generated from `.proto` |

## What is in here

```rust
use zaino_primitives::types::{Block, BlockHash, Height, Transaction, Treestate};
use zaino_primitives::types::rpc::{BlockDeltas, MiningInfo, NodeInfo, PeerInfo};
```

- `types` — the chain itself: `Block`, `BlockHeader`, `Transaction`,
  `BlockHash`, `TransactionHash`, `Height`, `BlockRef`, `TreeRoot`,
  `Treestate`, `ShieldedPool`, `ChainMetadata`, `Zatoshis`, `SignedZatoshis`.
- `types::rpc` — the response shapes for passthrough RPCs, in domain
  vocabulary rather than any interface's: `BlockDeltas`, `BlockchainInfo`,
  `ChainTip`, `MiningInfo`, `NodeInfo`, `PeerInfo`, `SpentInfo`, `TxOut`,
  `BlockSubsidy`.

### Bytes are allowed; JSON is not

Some types carry `Vec<u8>` — a raw block, a raw transaction, a serialized
commitment tree. Those are **Zcash protocol bytes**: the canonical
consensus-defined encoding that a hash commits to. They are in the domain
because they *are* domain facts, not because they are a convenient blob.

A `serde_json::Value` is a different thing entirely and does not belong here.
If a type needs one, the type belongs at a boundary.

## Invariants live in constructors

Types enforce what they claim:

```rust
let h = Height::try_from(800_000u32)?;   // rejects above 2^31 - 1
let z = Zatoshis::new(21_000_000)?;      // rejects out-of-range amounts
```

`Height::checked_add` / `checked_sub` are checked, not wrapping. Prefer
expressing an invariant in the type over asserting it at a call site — the
no-`unwrap` rule in CLAUDE.md is much easier to follow when the type has
already done the work.

## Byte order

Internal order throughout. `BlockHash` and `TransactionHash` hold bytes in the
order the protocol hashes them, **not** the reversed order used for display.
The reversal is a presentation concern and happens at the boundary that
presents:

```rust
// in an adapter or a wire module, never here
let displayed = { let mut b = <[u8; 32]>::from(hash); b.reverse(); hex::encode(b) };
```

Tree roots and nonces are **not** reversed for display. If you are unsure which
a field is, check the wire module that serves it — `wire/treestate.rs` and
`wire/subtrees.rs` each state their choice and pin it with a test.

## Related

- ADR-0008 — validator access is a set of single-question ports over these types.
- `zaino-source` — the ports themselves.
