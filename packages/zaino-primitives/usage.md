# zaino-primitives

Zaino's domain vocabulary: Zcash chain types in Zaino's own terms, independent
of how they are transported or stored. Every other Zaino crate that needs a
height, hash, block or amount depends on this one.

## Dependencies: `thiserror`, `sha2`, `uint`, `serde` and librustzcash's light crates only

Anything added here lands in every crate above it. `zcash_protocol`,
`zcash_transparent` and `zcash_script` carry the protocol types Zaino takes as-is
(heights, block hashes, txids, values, outpoints, scripts, upgrade schedules);
`zcash_encoding` for display-order hex (`Target::from_hex`); `sha2` for
`sha256d`: block hashes (`HeaderBytes::hash`) and `MerkleRoot::of_txids`, the
consensus merkle root a block's transactions must rebuild (the NFS's body
check, and the chain builder's headers); `uint` for the 256-bit `Target`
arithmetic (zebra's own U256). No domain type implements serde: formats are owned
by the boundary that speaks them. `serde` is here for `serde_secs` only: the
`#[serde(with = …)]` adapters every crate whose config type is a zainod table uses
for its whole-second `*_secs` keys (`any`, `nonzero`, `zero_is_never` = 0 means
`None`).

| Direction | Owner |
|---|---|
| validator JSON-RPC reply → domain | `zaino-source` (`parse.rs` for JSON, `decode.rs` for consensus bytes) |
| consensus header bytes → fields | `HeaderBytes` here (shared by `zaino-source` and `zaino-header-chain`) |
| domain → disk | each index crate's own `encode` / `decode` (`zaino-persistence` sees bytes only) |
| domain → gRPC | `zaino-grpc` and the index crates, onto `zaino-proto` types |

Fields may carry Zcash protocol bytes (raw blocks, transactions, serialized
trees): those are consensus-defined encodings, so they are domain facts. JSON
values never belong here.

## Modules

```rust
use zaino_primitives::protocol::{MAX_BLOCK_BYTES, MAX_BLOCK_REORG_HEIGHT};
use zaino_primitives::sha256d;
use zaino_primitives::types::{Block, HeaderBytes, Treestate};
```

- `types` — the chain: `Block` (the one decoded block every index consumes,
  on the finalized stream and in every NFS node; `at()` = its `BlockRef`), `DecodedHeader`
  (`extends(parent: Option<BlockRef>)` = one height above `parent` and linked to
  it by `prev_hash`, genesis on `None`: every fold's precondition), `Transaction` and its parts
  (`TransparentData`, `SaplingData`, `OrchardData`, `SproutData`, …; Sprout's
  value balance included; `TransparentData::coinbase` marks the coinbase), `Fee`
  (`Coinbase`, or `Paid` = what the transaction leaves in the transparent pool:
  `Transaction::fee(spent)`, `spent` = Σ its prevouts' values; `FeeError::Negative`
  or `Overflow`) and `BlockFees` (one `Fee` per transaction, named by block hash),
  `PeerTxId` (txid + v5 auth digest: how peers name an unmined transaction), `Announced`
  (a peer's block `inv`, its sender attached),
  `BlockRef`, `FinalityDepth`, `TreeSize` / `TreeSizes` (`PerPool<TreeSize>`),
  `TreeRoot`, `Treestate`, `SubtreeRoot`, `MerkleRoot`,
  `HeaderBytes` and `TransactionLocation`, plus `CompactDifficulty` below.
  What a validator reports about itself (`BlockchainInfo`, `PeerInfo`, …) is
  `zaino-source`'s, beside its parser.
- Upgrade schedule = `zcash_protocol`'s `LocalNetwork` (no Zaino schedule type):
  `protocol::compiled_schedule(network)` = mainnet's or testnet's (`None` for
  regtest, whose heights are set per chain); `zaino_source::BlockchainInfo::schedule()`
  = a validator's listing (regtest), keyed by branch id. Pool activations, the header
  rules (`target_spacing`, NU7's window) and `GetLightdInfo` all read it. Branch ids
  are `zcash_protocol`'s `BranchId`.
- `Block::check_body()` — its transactions are the ones its header commits to:
  `BodyMismatch::MerkleRoot` (txids rebuild another root) or `Mutated` (a repeated
  txid pair, CVE-2012-2459). With the hash matched, a body passing it is the one
  asked for, whoever sent it.
- `protocol` — `MAX_BLOCK_REORG_HEIGHT` (1000), `MAX_BLOCK_BYTES`
  (2,000,000, the spec's `MAX_BLOCK_SIZE`), `MEDIAN_TIME_SPAN` (11, the spec's
  `PoWMedianBlockSpan`) and `median_time_past(newest_first)` (upper median of
  the newest 11 times; `None` = none). Stated as protocol facts, not borrowed
  from a node. Restating them elsewhere is a bug.
- `sha256d` — SHA-256 twice, the one implementation (block hashes, merkle
  nodes, pre-v5 txids).

## Invariants live in constructors

```rust
let c = CompactCiphertext::prefix_of(&note);          // the 52-byte head of a note ciphertext
let (header, rest) = HeaderBytes::split(&raw)?;       // one consensus header + what follows
```

- Heights are librustzcash's `BlockHeight` (`H0` = genesis; `+` / `-` saturate,
  so a step that may cross genesis goes through `u32::checked_sub`).
- `Transaction` stores no index: position is list order, and transaction 0 is
  the coinbase.
- `TreeSize` is `u32`-backed (`TryFrom<u64>` refuses a full depth-32 tree,
  `TreeSizeOutOfRange`); `TreeSizes::advance(&block)` = the cumulative sizes
  after a block (one commitment per Sapling output, Orchard or Ironwood action; panics past
  2^32 leaves, which consensus never builds).
- `HeaderBytes::split` checks the layout (fixed fields present, solution 1344
  or regtest's 36 bytes behind a minimal compactsize; `HeaderError` names each
  fault) and reads every field from the bytes; its `hash()` is SHA-256d of
  those bytes, never a field taken on trust.

## Zatoshi values

Values are librustzcash's `zcash_protocol::value::Zatoshis` (an amount, `0` to `MAX_MONEY`) and
value balances its `ZatBalance` (`-MAX_MONEY` to `MAX_MONEY`). Coexisting balances cannot exceed
the supply, so their sum (`.sum::<Option<Zatoshis>>()`) is `None` only on double-counted input.

## Difficulty

`CompactDifficulty` is a header's nBits once validated. `try_from_bits(u32)`
applies a validator's acceptance rules (sign bit clear, target within 256 bits,
non-zero) plus the target's work fitting 128 bits, and reports each rejection
as its own `CompactDifficultyError` variant. A validated value gives its
`target()` and its `work()` (`floor(2^256 / (target + 1))`, always within
`u128`). This is the one nBits → target → work implementation; chain work and
the most-work rule built on it live in `zaino-header-chain`
([architecture §4](../../docs/architecture.md#4-the-best-chain)).

`Target` is the 256-bit threshold (zebra's `ExpandedDifficulty`), ordered
numerically: `is_met_by(hash)` (the hash read little-endian, ≤ target),
`to_compact()` (raw nBits, unvalidated: a target below `2^128` has no
`CompactDifficulty`), `mean(&[Target])` (overflow-free), `rescale(actual,
expected)` (`/ expected × actual`, divide first, saturating) and `from_hex` over
display-order hex (zcashd's `powLimit` literals).

## Byte order

Block hashes, txids, outpoints, scripts and zatoshi values are librustzcash's
(`zcash_protocol::BlockHash`, `zcash_protocol::TxId`,
`zcash_transparent::bundle::OutPoint`, …). Hashes hold internal (hash-output) byte
order (`.0` for a `BlockHash`'s bytes); `Display` / `as_hex` render the reversed
(RPC/explorer) hex and `from_hex` parses it back.

## Testing

This crate's types carry no test builder: chains of real consensus bytes come from
`zaino_source::testing::MockChain` ([guide](../zaino-source/usage.md#testing-mockchain)).
Its own unit tests build the few values they assert on inline.
