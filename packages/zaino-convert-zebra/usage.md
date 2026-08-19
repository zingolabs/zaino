# `zaino-convert-zebra` — usage

`zebra-chain` types → `zaino-primitives` domain types. One direction only.

## Use

```rust
use zaino_convert_zebra::{block_from_zebra, header_from_zebra, transaction_from_zebra};

let block = block_from_zebra(&zebra_block, height)?;
let tx = transaction_from_zebra(&zebra_tx, index_in_block)?;
```

All conversions return `Result<_, ConvertError>`. They are fallible because
`zebra-chain` types can hold values the domain types reject — a height above the
protocol maximum, an amount outside the valid range — and that check is the
point of the boundary, not an inconvenience at it.

## Why a separate crate

`zebra-chain` should appear in exactly one place below the adapters. Both
adapters need the same conversions:

- `zaino-source-zebra-rpc` deserializes raw block bytes with `zebra-chain` and
  converts the result.
- `zaino-source-zebra-readstate` gets `zebra-chain` types directly from the
  read-state service.

Without this crate the conversions get written twice, and the two copies drift —
which is how a field ends up populated on one transport and defaulted on the
other.

## One direction, deliberately

There is no `zebra_from_block`. Nothing needs it: the domain is what Zaino
serves from, and the places that still emit `zebra-rpc` shapes
(`z_getblock`, `getrawtransaction`) build them from block bytes plus chain
facts using zebra's own builders, so the formatting stays zebra's business.

If you find yourself wanting the reverse direction, check whether the caller
should be using the domain type instead.

## Related

- ADR-0008 — the crate graph, and why `zebra-chain` is confined below the adapters.
