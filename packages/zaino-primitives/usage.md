# zaino-primitives

Zaino's domain vocabulary: Zcash chain types in Zaino's own terms, independent of how they are
transported or stored. Every other Zaino crate that needs a height, hash, block or amount
depends on this one. The types themselves: the rustdoc of `types`, `protocol` and `sha256d`.

## Dependencies: `thiserror`, `sha2`, `uint`, `serde`, `metrics` and librustzcash's light crates only

Anything added here lands in every crate above it.

- `zcash_protocol`, `zcash_transparent` and `zcash_script` carry the protocol types Zaino takes
  as-is (heights, block hashes, txids, values, outpoints, scripts, upgrade schedules); there is no
  Zaino schedule type, only `zcash_protocol`'s `LocalNetwork`.
- `sha2` for `sha256d`, the one implementation (block hashes, merkle nodes, pre-v5 txids);
  `uint` for the 256-bit `Target` arithmetic.
- No domain type implements serde: formats are owned by the boundary that speaks them. `serde` is
  here for `serde_secs` only, the adapters every zainod config table uses for its whole-second
  `*_secs` keys.
- `metrics` is here for `metric::Metric` only: each emitting crate's `METRICS` table (name, kind,
  unit, help), which zainod walks once to register every metric.

| Direction | Owner |
|---|---|
| validator JSON-RPC reply → domain | `zaino-source` (`json.rs` for JSON, `decode.rs` for consensus bytes) |
| consensus header bytes → fields | `HeaderBytes` here (shared by `zaino-source` and `zaino-header-chain`) |
| domain → disk | each index crate's own `encode` / `decode` (`zaino-persistence` sees bytes only) |
| domain → gRPC | `zaino-grpc` and the index crates, onto `zaino-proto` types |

Fields may carry Zcash protocol bytes (raw blocks, transactions, serialized trees): those are
consensus-defined encodings, so they are domain facts. JSON values never belong here.

## Invariants live in constructors

- Heights are librustzcash's `BlockHeight` (`+` / `-` saturate, so a step that may cross genesis
  goes through `u32::checked_sub`).
- `Transaction` stores no index: position is list order, and transaction 0 is the coinbase.
- `HeaderBytes::split` checks the layout and reads every field from the bytes; its `hash()` is
  SHA-256d of those bytes, never a field taken on trust.
- `Block::check_body()`: its transactions are the ones its header commits to (`MerkleRoot`, or
  `Mutated` for a repeated txid pair, CVE-2012-2459). With the hash matched, a body passing it is
  the one asked for, whoever sent it.
- `TreeSize` is `u32`-backed; `TreeSizes::advance(&block)` panics past 2^32 leaves, which
  consensus never builds.
- `protocol`'s constants (`MAX_BLOCK_REORG_HEIGHT`, `MAX_BLOCK_BYTES`, `MEDIAN_TIME_SPAN`) are
  stated as protocol facts, not borrowed from a node. Restating them elsewhere is a bug.

## Values and difficulty

Values are librustzcash's `Zatoshis` (`0` to `MAX_MONEY`) and value balances its `ZatBalance`.
Coexisting balances cannot exceed the supply, so their sum (`.sum::<Option<Zatoshis>>()`) is
`None` only on double-counted input.

`CompactDifficulty` is a header's nBits once validated (`try_from_bits` applies a validator's
acceptance rules, each rejection its own error variant), and the one nBits → target → work
implementation. `Target` is the 256-bit threshold (zebra's `ExpandedDifficulty`). Chain work and
the most-work rule built on them live in `zaino-header-chain`
([architecture §4](../../docs/architecture.md#4-the-best-chain)).

## Byte order

Block hashes, txids and outpoints are librustzcash's. Hashes hold internal (hash-output) byte
order; `Display` / `as_hex` render the reversed (RPC/explorer) hex and `from_hex` parses it back.

## Testing

This crate's types carry no test builder: chains of real consensus bytes come from
`zaino_source::testing::MockChain` ([guide](../zaino-source/usage.md#testing-mockchain)).
