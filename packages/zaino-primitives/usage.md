# zaino-primitives

Zaino's domain vocabulary: Zcash chain types in Zaino's own terms, independent
of how they are transported or stored. Every other Zaino crate that needs a
height, hash, block or amount depends on this one.

## Dependencies: `thiserror` only

Anything added here lands in every crate above it. In particular there is no
serde: formats are owned by the boundary that speaks them.

| Direction | Owner |
|---|---|
| validator JSON-RPC reply → domain | `zaino-source-zebra-rpc` (`parse.rs`, `convert.rs`) |
| domain → disk | each index crate's `Persistent*` records (`zaino-persistence`) |
| domain → gRPC | `zaino-grpc` and the index crates, onto `zaino-proto` types |

Fields may carry Zcash protocol bytes (raw blocks, transactions, serialized
trees): those are consensus-defined encodings, so they are domain facts. JSON
values never belong here.

## Modules

```rust
use zaino_primitives::protocol::{MAX_BLOCK_BYTES, MAX_BLOCK_REORG_HEIGHT};
use zaino_primitives::types::{Block, BlockHash, Height, TransactionId, Treestate};
use zaino_primitives::types::rpc::{BlockDeltas, MiningInfo, NodeInfo, PeerInfo};
```

- `types` — the chain: `Block` (the one decoded block every index
  consumes, off `zaino_sync::BlockSink`), `BlockHeader`, `ChainMetadata` (cumulative tree sizes, derived by
  the compact-block index), `Transaction` (and `types::transaction` parts,
  Sprout's value balance included; `TransparentData::coinbase` marks the
  coinbase), `Fee` (`Coinbase`, or `Paid` = what the transaction leaves in the
  transparent pool) and `BlockFees` (one `Fee` per transaction, named by block
  hash), `OutPoint`
  (`txid` + `vout`: a transparent input is the outpoint it spends, and the key
  both transparent indexes store under),
  `BlockHash`, `TransactionId`, `Height`, `BlockRef`, `TreeSize`, `TreeRoot`,
  `Treestate`, `SubtreeRoot`, `ShieldedPool`, `BlockchainInfo` and the
  network-upgrade types, plus the zatoshi and work families below.
- `types::rpc` — domain answers to `zaino-source` ports for validator queries
  (`BlockDeltas`, `BlockHeaderVerbose`, `BlockSubsidy`, `ChainTip`,
  `MiningInfo`, `NodeInfo`, `PeerInfo`, `SpentInfo`, `TxOut`, …). Only named,
  typed fields; `Option` means "the validator may not report it".
- `protocol` — `MAX_BLOCK_REORG_HEIGHT` (1000) and `MAX_BLOCK_BYTES`
  (2,000,000, the spec's `MAX_BLOCK_SIZE`). Stated as protocol facts, not
  borrowed from a node. Restating them elsewhere is a bug.

## Invariants live in constructors

```rust
let h = Height::try_from(800_000u32)?;                // ≤ 2^31 - 1
let z = Zatoshis::new(21_000_000)?;                   // ≤ money supply
let b = Block::try_new(header, txs)?;                 // non-empty tx list
let c = CompactCiphertext::try_new(&bytes)?;          // exactly 52 bytes
```

- `Height::checked_add` / `checked_sub` are checked, never wrapping.
- `Transaction` stores no index: position is list order, and
  `Block::coinbase()` is transaction 0.
- `CompactCiphertext` is the 52-byte compact head of a note ciphertext;
  once built it converts infallibly to `[u8; 52]`.
- `TreeSize::checked_add` enforces the compact protocol's `u32` range
  (`TreeSizeOutOfRange`); `TreeSizes::advance(&block)` = the cumulative sizes
  after a block (one commitment per Sapling output, Orchard or Ironwood action).

## Zatoshi family

| Type | Range (both ends inclusive) | Is |
|---|---|---|
| `Zatoshis` | `0` to `supply` | an amount: balance, UTXO value, one movement |
| `ZatoshisFlowSum` | `0` to `u128::MAX` | a sum of movements (not supply-bounded) |
| `SignedZatoshis` | `-supply` to `supply` | a signed movement or difference |

```rust
let received = ZatoshisFlowSum::try_accumulate(outputs.iter().copied())?; // None only past u128::MAX
let lifetime = ZatoshisFlowSum::from_summed(total_u64);                    // source-summed total
let net: Option<SignedZatoshis> = received.net(spent);                     // None if incoherent
let total: Option<Zatoshis> = Zatoshis::sum_balances(balances.iter().copied()); // coexisting balances; None past supply
let parsed = SignedZatoshis::new(value_i64)?;                          // boundary input
```

Movements recount the same coins, so their sum is its own type; coexisting
balances cannot exceed the supply, so `sum_balances` lands back in `Zatoshis`
and a total past the supply means double-counted input.

## Work family

| Type | Is |
|---|---|
| `SingleBlockWork` | expected work of one block, from its difficulty target |
| `AbsoluteChainWork` | total work up to a block (validators' `chainwork`) |

```rust
let mut total = AbsoluteChainWork::genesis(block_work);
total = total.accumulate(next_work)?; // WorkOverflow
total = total.rollback(next_work)?;   // WorkUnderflow
```

- `AbsoluteChainWork::try_from_reported([u8; 32])` reads a validator's
  big-endian value (`Ok(None)` when it does not track it); `to_be_bytes`
  renders it back. `AbsoluteChainWork::new(NonZeroU128)` and
  `SingleBlockWork::try_new(u128)` take an integer already held.
- `CompactDifficulty` (header nBits): `try_from_bits(u32)` /
  `try_from_be_bytes([u8; 4])` apply a validator's acceptance rules (sign bit
  clear, target within 256 bits, non-zero) plus work fitting 128 bits, each
  rejection its own `CompactDifficultyError` variant. `to_work()` returns the
  precomputed `SingleBlockWork` (`floor(2^256 / (target + 1))`); `as_bits()`
  reads the raw `u32`. The arithmetic is native; `zaino-source-zebra-rpc`'s
  `convert` tests sweep it against Zebra.

## Byte order

`BlockHash` and `TransactionId` hold internal (hash-output) byte order and
convert via `From<[u8; 32]>` both ways. Their `Display` renders the reversed
(RPC/explorer) hex; any other display-order rendering happens at the boundary
that presents it.

## Features

`testing` exposes `BlockHeader::for_tests(height, hash, prev_hash, time)`: a
fixture header with regtest `bits` and every field no test asserts on zeroed.
Enable it from `[dev-dependencies]` only.
