# `zaino-consensus` — usage

Zcash consensus constants, and the protocol-limit validation built on them. A
leaf crate that depends on no node implementation.

## Why the values are stated here rather than borrowed

These are protocol facts. Anything reasoning about the chain needs them,
including subsystems built to depend on as little as possible, so holding them in
a general-purpose crate meant referencing a reorg bound cost a dependency on the
config, logging and TLS stacks too.

They are also *not* any implementation's values. Zebra encodes the consensus
rules exactly as this crate does; it does not define them. Re-exporting
`zebra_chain::block::MAX_BLOCK_BYTES` would take a dependency on a peer's reading
of a specification we can read ourselves, and drag that peer's entire type system
along for a `u64`.

So each value is stated here with its provenance in the protocol specification.

## What makes that safe: the agreement tests

Independence is only safe if the two readings are checked against each other
somewhere. That somewhere is `zaino-convert-zebra`, which already owns our
relationship to zebra's types, and takes `zaino-consensus` as a
**dev-dependency** so the check costs nothing at build time. See its
`consensus_agreement` module: it asserts the constants match, and sweeps
`work_from_bits` against zebra's implementation across 256 exponents × 8
mantissas chosen to sit on the boundaries where the two could plausibly disagree.

A failure there does not say which side is wrong. It says the protocol moved or
one of us misread it, and that the answer needs looking up in the specification
rather than copied across. **Do not fix it by changing our value to match
zebra's** without reading the spec — that turns the test into a mirror.

If you add a constant here, add its agreement test there in the same change.

## Use

```rust
use zaino_consensus::{
    validate_raw_transaction_hex, work_from_bits, COINBASE_MATURITY,
    MAX_BLOCK_BYTES, MAX_NONFINALISED_DEPTH,
};
```

- `MAX_NONFINALISED_DEPTH` is the finalised / non-finalised seam:
  `MAX_BLOCK_REORG_HEIGHT + 1`, the `+ 1` accounting for the tip block itself.
- `FAST_TEST_MAX_NONFINALISED_DEPTH` is a tenth of it, for tests that need a
  moving seam without building a ~1001-block chain. It is a test convenience,
  never a production value: in `zaino-state` the choice between the two is
  `cfg(test)` / the `fast-test-seam` feature, and both arms derive from the
  protocol constant so neither is a literal.
- `validate_raw_transaction_hex` runs before the validator round trip, so a
  malformed or oversized submission is rejected locally. It distinguishes bad
  hex from an oversized transaction — a client that sent a truncated string
  needs to know that, not that its transaction was too big.
- `work_from_bits` expands a compact `nBits` target and returns
  `floor(2^256 / (target + 1))`. It **rejects** malformed encodings — negative
  mantissa, zero target, overflowing exponent — rather than clamping them,
  because that is what a validator does before it compares the hash. Agreement
  on which encodings are invalid matters as much as agreement on the values.

## Keep the dependency list where it is

`thiserror`, `hex`, and `primitive-types` (256-bit arithmetic for target
expansion). Adding a node implementation here would defeat the point of the
crate; adding a general-purpose one would recreate the problem it was extracted
from.

## Related

- `zaino-status` — the other leaf extracted from `zaino-common` for the same
  reason.
- `packages/zaino-convert-zebra/src/lib.rs` — the `consensus_agreement` tests.
