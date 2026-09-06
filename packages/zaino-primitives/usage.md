# `zaino-primitives` — usage

Zaino's domain vocabulary: the types that describe the Zcash chain in Zaino's
own terms, independent of how any of it is transported or stored.

## The one rule

**This crate depends only on `thiserror` and the librustzcash
protocol-specification crates (`zcash_address`, `zcash_protocol`).** Keeping the
list this short is not an accident of the current implementation — it is the
property that makes every other crate able to depend on it. Adding a dependency
here adds it to `zaino-source`, both adapters, `zaino-state`, `zaino-serve` and
`zainod` at once, so each addition has to earn its place.

The rule is about *what a dependency lets in*, not a fixed name list. Two things
must never enter through it, because both would let some other layer's decisions
leak into the domain vocabulary:

- **No serialization framework.** In particular there is **no serde**. A serde
  derive in this crate would let the wire format and the domain model start
  deciding each other, which is exactly what ADR-0009 exists to prevent.
- **No node, transport, or storage implementation.** A validator client, a gRPC
  or JSON-RPC stack, a database — these belong at the boundaries that own them,
  never in the vocabulary every boundary shares.

A **protocol-specification** library is admissible, on one condition: its API is
**fully contained** — no type of its own appears in any public signature,
re-export, or public error of this crate. Validation happens inside; everything
exposed is ours. `zcash_address` is the first such dependency, used by
[`TransparentAddress`](src/types/transparent_address.rs) to decide whether a
string is a valid transparent address. The alternative was to hand-roll
Base58Check and SHA-256 here, which buys risk, not ownership: the address
encoding *is* the protocol, and the spec-steward's parser is the spec artifact,
so re-implementing it would mean maintaining a second, drift-prone copy of a
consensus rule. We take the parser and keep its types off our surface — the
containment condition is what makes that a domain decision expressed in our own
vocabulary rather than a leak of theirs.

Serialization, by contrast, lives at the boundary that owns the format:

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
  `Treestate`, `ShieldedPool`, `ChainMetadata`, and the zatoshi quantity
  family `Zatoshis` / `ZatoshisFlowSum` / `SignedZatoshis` (see below).
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
let a = TransparentAddress::try_new(s)?; // rejects non-transparent / undecodable
```

`TransparentAddress` is network-blind on construction: it accepts a valid
transparent address for any network and reports which one via `network()` (an
`AddressNetwork`) and the script form via `script_type()`. The primitive states
what the address *is*; whether that network is the one a query should act on is
the consumer's policy, not the address's invariant.

`Height::checked_add` / `checked_sub` are checked, not wrapping. Prefer
expressing an invariant in the type over asserting it at a call site — the
no-`unwrap` rule in CLAUDE.md is much easier to follow when the type has
already done the work.

## The zatoshi quantity family

Three types share the zatoshi unit but carry different invariants, so summing
and differencing amounts is done through them rather than a bare integer. See
ADR-0013 for the doctrine.

| type | range | is |
|---|---|---|
| `Zatoshis` | `0 ..= supply` | an amount held — a balance, a UTXO value |
| `ZatoshisFlowSum` | `0 ..= u128::MAX` | an accumulation of movements, **not** supply-bounded |
| `SignedZatoshis` | `-supply ..= supply` | a signed value: a movement or a difference |

A sum of *movements* — every output paying an address, every input it spent —
counts the same coins each time they move, so it is not bounded by the supply;
that is why it is its own type and not another `Zatoshis`. A sum of *coexisting*
balances stays supply-bounded — coins that coexist cannot total more than
exist — so `Zatoshis` is closed under it and there is no fourth type: that sum
is the operation `Zatoshis::sum_balances`, landing back in `Zatoshis`.

The operations relate the types and live beside them:

```rust
use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum, SignedZatoshis};

// Sum amounts as flow. `None` only on machine overflow (unreachable in
// practice), never on passing the supply — gross flow legitimately can.
let received = ZatoshisFlowSum::try_accumulate(outputs.iter().copied())?;
let spent = ZatoshisFlowSum::try_accumulate(spends.iter().copied())?;

// Adopt a flow total a backend delivered already summed as a u64.
// Infallible: a u64 always fits the u128 accumulator, and machine
// representability is the flow sum's only invariant.
let lifetime = ZatoshisFlowSum::from_summed(received_total);

// Net of a received flow minus a spent flow for one balance, as a signed
// value. `None` if the two flows don't describe a coherent balance.
let net: Option<SignedZatoshis> = received.net(spent);

// Sum balances that coexist at one moment. Supply-capped and closed: the
// total lands back in `Zatoshis`. `None` means the total passed the supply,
// which under the coexistence contract is overlapping or double-counted
// input, not a large number.
let held: Option<Zatoshis> = Zatoshis::sum_balances(balances.iter().copied());
```

`ZatoshisFlowSum` has two validated doors and no unchecked one:
`try_accumulate` for a total *derived* in the domain as the checked sum of
some amounts, and `from_summed` for a total a source *delivers already
summed*. `SignedZatoshis` likewise — `ZatoshisFlowSum::net` for a value
*derived* in the domain, and `SignedZatoshis::try_new` for one *parsed at a
boundary* (a movement read off the wire or disk). `try_new` is the
external-input validation step for a signed value, the same discipline the
crate applies at every wire and persistence boundary, pushed down to the
primitive.

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
