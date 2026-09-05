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
```

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
balances would be supply-bounded, and is a real fourth member of this family,
but has no consumer yet and is not built (it is named in the arithmetic
module's docs).

The operations relate the types and live beside them:

```rust
use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum, SignedZatoshis};

// Sum amounts as flow. `None` only on machine overflow (unreachable in
// practice), never on passing the supply — gross flow legitimately can.
let received = ZatoshisFlowSum::try_accumulate(outputs.iter().copied())?;
let spent = ZatoshisFlowSum::try_accumulate(spends.iter().copied())?;

// Net of a received flow minus a spent flow for one balance, as a signed
// value. `None` if the two flows don't describe a coherent balance.
let net: Option<SignedZatoshis> = received.net(spent);
```

`ZatoshisFlowSum` has no other constructor: a flow sum is only ever the checked
sum of some amounts. `SignedZatoshis` has two validated doors and no unchecked
one — `ZatoshisFlowSum::net` for a value *derived* in the domain, and
`SignedZatoshis::try_new` for one *parsed at a boundary* (a movement read off the
wire or disk). `try_new` is the external-input validation step for a signed
value, the
same discipline the crate applies at every wire and persistence boundary,
pushed down to the primitive.

## The work quantity family

The same doctrine, applied to proof-of-work. Two quantities share the unit and
are not interchangeable:

| type | is |
|---|---|
| `BlockWork` | the expected work of **one** block, from its difficulty target |
| `ChainWork` | **cumulative** work at a block — the fold of block works along its chain; `Ord`, because comparing it *is* chain selection |

Both are strictly positive. The fold is the algebra, in the `work::arithmetic`
module: `ChainWork::genesis(block_work)` seeds it (genesis's cumulative work is
its own block work), `accumulate` extends it, `rollback` unwinds it on reorg —
each checked, with a typed error. There is deliberately no
`ChainWork + ChainWork`: no chain is the concatenation of two chains.

Boundary doors on `ChainWork`: `try_from_reported` reads the 32 big-endian
bytes a validator reports — all-zero is `Ok(None)` ("not reported"; absence is
`Option`, never a zero sentinel) and a value past the recorded 128 bits is
refused rather than truncated — and `to_be_bytes` renders back for the wire.
`try_new` / `BlockWork::try_new` take an already-computed integer and enforce
only the non-zero bound.

### Where `BlockWork` comes from: `CompactDifficulty`

The nBits encoding from the block header is its own validated type,
`CompactDifficulty`, and the whole bits → target → work conversion is native
to this crate — the domain owns its arithmetic, and consensus implementations
serve as *differential-test oracles* (`zaino-convert-zebra` sweeps the
pipeline against zebra across the encoding space) rather than as dependencies.

Construction is only through checked doors — `try_from_bits(u32)` for a value
carried numerically, `try_from_be_bytes([u8; 4])` for one carried as its
display-order bytes. Both apply the acceptance set a validator enforces before
comparing a hash (clear sign bit, target within 256 bits, non-zero target),
with one typed `CompactDifficultyError` variant per rejected rule. `as_bits`
reads the raw `u32` back out for wire and persistence renders.

`to_work()` derives the block's `BlockWork` — `floor(2^256 / (target + 1))` —
and stays fallible on a *valid* encoding: validity is a property of the
256-bit target, but work is recorded in 128 bits, and the encoding admits
targets below `2^128` whose work does not fit. No block from a real chain
trips `WorkOverWidth`; a value that does did not come from one. The expanded
256-bit target itself never leaves the type: no consumer reasons about
targets, only about validity and work.

## Confirmation state

Tip-relative confirmation state is an enum pair, not an integer. The RPC
interface flattens it into one signed number (`-1` not on the best chain, `0`
mempool, `n ≥ 1` depth + 1); in the domain that integer exists only at the
wire.

| type | states | subject |
|---|---|---|
| `BlockConfirmations` | `NotInBestChain` \| `Confirmed(NonZeroU32)` | a block |
| `TxConfirmations` | `Mempool` \| `Mined(BlockConfirmations)` | a transaction and its outputs |

Two types because the state spaces differ: a block is never in the mempool,
and a single three-state enum would force a dead `Mempool` arm on every
block-side consumer. The sharing is vertical — a mined transaction's state
*is* its block's — so `TxConfirmations` embeds the block type and forwards
`count()` / `is_in_best_chain()` through `Mined`. There is deliberately no
trait over the two; if a consumer generic over both ever appears, extract one
then.

```rust
use zaino_primitives::types::{BlockConfirmations, Height, TxConfirmations};

// The off-by-one lives here and nowhere else: the tip is Confirmed(1).
// A height above the tip (a caller racing a tip update) clamps to
// Confirmed(1) — the contract is on the constructor's docs.
let state = BlockConfirmations::of_best_chain_block(height, tip);

// The wire codec pair on each type. Parsing is the external-input
// validation step: 0 on the block door, anything below -1, and counts
// past u32 are rejected with a typed ConfirmationsCodecError.
let n = state.to_rpc_i64();
let back = BlockConfirmations::try_from_rpc_i64(n)?;
let tx = TxConfirmations::try_from_rpc_i64(0)?; // Mempool
```

`Height::depth_from(tip)` is the single home for the underlying "tip − height"
subtraction (`None` when the height is above the tip).

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
