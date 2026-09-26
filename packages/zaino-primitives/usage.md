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

A **protocol-specification** library is admissible, on one condition: its
machinery stays **contained** — its parsers and errors never appear in a public
signature, re-export, or public error of this crate. Validation happens inside.
A plain protocol enum may appear where it names the protocol concept directly:
`TransparentAddress::network()` returns `zcash_protocol`'s `NetworkType` rather
than a Zaino copy of it. `zcash_address` is the first such dependency, used by
[`TransparentAddress`](src/types/transparent_address.rs) to decide whether a
string is a valid transparent address. The alternative was to hand-roll
Base58Check and SHA-256 here, which buys risk, not ownership: the address
encoding *is* the protocol, and the spec-steward's parser is the spec artifact,
so re-implementing it would mean maintaining a second, drift-prone copy of a
consensus rule. We take the parser and keep its machinery off our surface — the
containment condition is what makes that a domain decision rather than a leak of
theirs.

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
  `Treestate`, `ShieldedPool`, `ChainMetadata`, `CompactDifficulty`, the work
  quantity family `SingleBlockWork` / `AbsoluteChainWork`, the zatoshi quantity
  family `Zatoshis` / `ZatoshisFlowSum` / `SignedZatoshis`, and the
  transparent-script family `Script` / `ScriptType` / `classify_script` /
  `TransparentAddressKey` / `TransparentAddress` (all four families below).
- `types::rpc` — the response shapes for node-forwarding RPCs, in domain
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
let b = Block::try_new(header, txs, chain_metadata)?; // rejects an empty tx list
let c = CompactCiphertext::try_new(&bytes)?; // rejects anything but exactly 52 bytes
let a = TransparentAddress::try_new(s)?; // rejects non-transparent / undecodable
```

A transaction's position is the block's to know, not the transaction's:
`Transaction` stores no index, and coinbase-ness is read from block order via
`Block::coinbase()` (position 0), never from a per-transaction field that could
disagree with the container. The list itself is fixed at construction:
`Block::transactions()` lends it as a slice, and no holder of a `Block` can
add, drop, or reorder a transaction after `try_new` has accepted it.

`CompactCiphertext` is the 52-byte compact head of a note ciphertext — the
form a compact transaction serves to light clients, not the full 580-byte
encryption output. Once constructed it converts infallibly to `[u8; 52]`, so
no consumer re-checks the width.

`TransparentAddress` is network-blind on construction: it accepts a valid
transparent address for any network and reports which one via `network()` (a
`zcash_protocol` `NetworkType`) and the script form via `script_type()`. The
primitive states what the address *is*; whether that network is the one a query
should act on is the consumer's policy, not the address's invariant.

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
| `Zatoshis` | `0 ..= supply` | an amount of ZEC counted in zatoshis — a balance, a UTXO value, a single movement |
| `ZatoshisFlowSum` | `0 ..= u128::MAX` | an accumulation of movements, **not** supply-bounded |
| `SignedZatoshis` | `-supply ..= supply` | a signed value: a movement or a difference |

A sum of *movements* — every output paying an address, every input it spent —
counts the same coins each time they move, so it is not bounded by the supply;
that is why it is its own type and not another `Zatoshis`. A sum of *coexisting*
balances stays supply-bounded — coins that coexist cannot total more than
exist — so that precondition keeps the total inside `Zatoshis` and there is no
fourth type: that sum is the operation `Zatoshis::sum_balances`, a checked fold
landing back in `Zatoshis`. The set of `Zatoshis` is still not closed under
addition; the fold refuses a total past the supply rather than pretend the
precondition held.

The operations relate the types and live beside them:

```rust
use zaino_primitives::types::{Zatoshis, ZatoshisFlowSum, SignedZatoshis};

// Sum amounts as flow. `None` only past `u128::MAX` (unreachable in
// practice), never on passing the supply — gross flow legitimately can.
let received = ZatoshisFlowSum::try_accumulate(outputs.iter().copied())?;
let spent = ZatoshisFlowSum::try_accumulate(spends.iter().copied())?;

// Adopt a flow total a backend delivered already summed as a u64.
// Infallible: a u64 always fits the u128 accumulator, and u128::MAX
// is the flow sum's only bound.
let lifetime = ZatoshisFlowSum::from_summed(received_total);

// Net of a received flow minus a spent flow for one balance, as a signed
// value. `None` if the two flows don't describe a coherent balance.
let net: Option<SignedZatoshis> = received.net(spent);

// Sum balances that coexist at one moment. Supply-capped, and under that
// precondition the total lands back in `Zatoshis`. `None` means the total
// passed the supply, which under the coexistence contract is overlapping or
// double-counted input, not a large number.
let total: Option<Zatoshis> = Zatoshis::sum_balances(balances.iter().copied());
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

## Keying a transparent output

A transparent output is locked by a script, and every index that answers
"what happened to this address?" has to turn that script into a key. The types
that do it live here, together, because more than one subsystem applies the
rule and they have to apply the same one.

| type | is |
|---|---|
| `Script` | the raw locking bytes, as they appear in the output |
| `ScriptType` | which standard form those bytes take — `P2PKH`, `P2SH`, or `NonStandard` |
| `TransparentAddressKey` | the pair an index keys by: 20 bytes plus the form they came from |
| `TransparentAddress` | the encoded, network-specific string a user sees |

`classify_script` is the rule itself, and `TransparentAddressKey::from_script`
is the door producers should use:

```rust
use zaino_primitives::types::TransparentAddressKey;

// The only constructor a producer needs. Shares `classify_script` rather than
// restating it, so a store indexing an output and a chain head reporting on
// one cannot classify the same script differently.
let key = TransparentAddressKey::from_script(&script_bytes);

// False means the 20 bytes are an index key and nothing more: they will not
// round-trip to a script, and two non-standard outputs can collide on them.
if key.is_standard() { /* reconstructible as an address, given a network */ }
```

The rule is **total**: every script gets a key, including scripts with no
address at all. An index that refused non-standard outputs would answer "no
history" for an address that has some, so `NonStandard` is a real answer rather
than a failure. What an index then *does* with it is the index's business —
Zaino's transparent-address history keys them, while its UTXO-set accumulator
excludes them, mirroring zcashd's `IsUnspendable`.

### Why the key is not the address

Two reasons, and they are why address-history surfaces are typed on
`TransparentAddressKey` rather than on `TransparentAddress`.

An index must be able to name outputs that have no address, per the totality
above. And an address string is network-specific — `t1`/`t3` on mainnet,
`tm`/`t2` on testnet, the same 20 bytes either way — so encoding one requires a
network, and encoding is a serving concern. It lives in `zaino-address`, which
the serving layer depends on and the chain layer does not: encoding drags in the
whole `zcash_address` / `zcash_keys` / `sapling-crypto` stack, and the chain
layer has no business carrying it. Keying on the hash keeps it out entirely.

`ScriptType` deliberately carries no discriminants. The on-disk tag values
belong to whichever backend writes them, so a second backend can choose its own
without this crate having already decided; a backend maps to and from its own
tags at its persistence boundary.

## The work quantity family

Three quantities share the proof-of-work unit and are not interchangeable:

| type | is |
|---|---|
| `SingleBlockWork` | the work **one** block is expected to take, from its difficulty target |
| `AbsoluteChainWork` | the **total** work of a chain up to a block — what validators report as `chainwork` |
| `RelativeChainWork` | the work a **run of blocks** holds, measured from wherever the run begins |

Each fold is a method on the type it returns, and each is checked:

```rust,ignore
// From genesis. A chain of one block has that block's work.
let mut total = AbsoluteChainWork::genesis(block_work);
total = total.accumulate(next_block_work)?;   // extend
total = total.rollback(next_block_work)?;     // unwind, on reorg

// Over a run. The empty run has accumulated nothing.
let mut run = RelativeChainWork::ZERO;
run = run.accumulate(next_block_work)?;
```

Nothing converts between `AbsoluteChainWork` and `RelativeChainWork`. A consumer
that can only observe a run of blocks holds the relative type and compares runs
against each other. The `types::work` module documentation states the algebra
and why the three are distinct.

`to_be_bytes` renders the 32 big-endian byte form and `from_be_bytes` reads it
back, refusing an over-width or all-zero value with `ChainWorkBytesError`; the
store's row is that form. Nothing reads chainwork off the wire, since Zebra does
not report it. For an integer you already hold, use
`AbsoluteChainWork::new(NonZeroU128)` or `SingleBlockWork::new(NonZeroU128)`.

The `types::work` module documentation states the full algebra, including what
`AbsoluteChainWork` is *not* — in particular `zaino-chain-head`'s
anchor-relative work, which is a third quantity.

### Where `SingleBlockWork` comes from: `CompactDifficulty`

The nBits encoding from the block header is its own validated type,
`CompactDifficulty`, and the whole bits → target → work conversion is native
to this crate — the domain owns its arithmetic, and consensus implementations
serve as *differential-test oracles* (`zaino-convert-zebra` sweeps the
pipeline against zebra across the encoding space) rather than as dependencies.

Construction is only through checked doors — `try_from_bits(u32)` for a value
carried numerically, `try_from_be_bytes([u8; 4])` for one carried as its
display-order bytes. Both apply the acceptance set a validator enforces before
comparing a hash (clear sign bit, target within 256 bits, non-zero target),
plus one domain rule: the target's work must fit the 128 bits work is recorded
in. Each rejected rule has its own `CompactDifficultyError` variant. `as_bits`
reads the raw `u32` back out for wire and persistence renders.

The work — `floor(2^256 / (target + 1))` — is computed once at construction,
so `to_work()` is an infallible getter returning the block's
`SingleBlockWork`. The expanded 256-bit target itself never leaves the type:
no consumer reasons about targets, only about validity and work.

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
