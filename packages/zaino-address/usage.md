# `zaino-address` — usage

Zcash address classification and unified-address decomposition. A leaf crate
serving `validateaddress`, `z_validateaddress` and `z_listunifiedreceivers`.

## Why it is its own crate

Address classification is pure parsing over `zcash_address` / `zcash_keys` /
`zcash_transparent` / `sapling-crypto`, with no chain access at all. That
dependency set is substantial and nothing else in Zaino wants it.

- Not `zaino-primitives`: that crate depends only on `thiserror`, and the whole
  point of it is that everything can depend on it. Adding the address stack
  there would put it in every crate in the workspace.
- Not `zaino-common`: that is config, logging, net, status, xdg — shared
  infrastructure. Address classification is domain logic, and it would force the
  zcash address stack on every crate that merely wants a `ServiceConfig`.

As a leaf below `zaino-state` it isolates the dependency, and it is the natural
home for validating address *parameters* on `getaddressbalance`,
`getaddressutxos` and `getaddressdeltas` when that work happens.

## Use

```rust
use zaino_address::{list_unified_receivers, validate_address, z_validate_address};

let result = validate_address(address_string, network);
let result = z_validate_address(address_string, network);
let receivers = list_unified_receivers(address_string, network);
```

The classification entry points return domain types (`ValidatedAddress`,
`ZValidatedAddress`) with **no serde**. The legacy-shaped JSON — including the
exact field sets, which differ per address kind — is `zaino-serve`'s
`wire/address.rs`, per ADR-0009.

`list_unified_receivers` backs `z_listunifiedreceivers`: it decomposes a unified
address into the receivers it bundles, each re-encoded as a standalone address a
caller can pay to (the transparent and Sapling receivers directly, the Orchard
receiver as a unified address carrying only it). It returns `Some(UnifiedReceivers)`
for a unified address on the queried network and `None` for anything else — a
definitive answer, since the caller named a specific string. Like the two
classification functions it reads no chain state and is generic over the network
`Parameters`. `UnifiedReceivers` is a plain domain struct with no serde; the wire
shape lives in the serving adapter.

### `orchard` feature is load-bearing

`Cargo.toml` enables both `orchard` and `sapling` on `zcash_keys`. The `orchard`
feature is not optional for correctness: `UnifiedAddress::orchard()` and
`from_receivers`' Orchard parameter are `#[cfg(feature = "orchard")]`, so without
it the Orchard receiver is unreachable. Most modern unified addresses are
Orchard-primary, so dropping the feature would make `list_unified_receivers`
under-report the common case. The `an_orchard_receiver_is_reported_not_silently_dropped`
test guards this.

## What is deliberately not classified

**Sprout.** `validate_address` and `z_validate_address` both report a Sprout
address as invalid, and `ZValidatedAddress` has no Sprout variant.

This is not a regression introduced by extracting the crate: the previous
implementation already fell through to `invalid()` for Sprout, with the comment
*"It could be the case that Zaino needs to support Sprout. For now, it's been
disabled."* What changed is that the *type* now says so, instead of a dead wire
variant implying support that the classifier never produced.

Zaino does not serve Sprout data anywhere else either. If that changes, add the
variant here and in `wire/address.rs` together.

## Decoding a script to an address

`transparent_address_from_script(script, params)` decodes a transparent output's
locking script to the address it pays **and which standard template it is**, for
the explorer's `scriptPubKey.addresses` and `scriptPubKey.type`:

```rust
use zaino_address::{transparent_address_from_script, ScriptAddress, TransparentScriptKind};

let decoded: Option<ScriptAddress> = transparent_address_from_script(&script_bytes, network);
// decoded.kind is PubKeyHash | ScriptHash; decoded.address is the encoded string.
```

Only the two standard templates decode: the 25-byte pay-to-public-key-hash
(`t1…`, `PubKeyHash`) and the 23-byte pay-to-script-hash (`t3…`, `ScriptHash`).
Every other script — multisig, `OP_RETURN`, a malformed template — returns
`None`, because it is not an address a user controls.

The kind travels with the address so a caller labels `scriptPubKey.type` from the
same decision — it never re-inspects the script bytes to tell P2PKH from P2SH.

## Disassembling a script to `asm`

`script_to_asm(script, decode_sighash)` renders a transparent script as zcashd's
`asm` string, for `scriptPubKey.asm` and `vin[].scriptSig.asm`. It is exact for
the standard templates (data pushes as hex, the small-number opcodes as decimals,
named opcodes as `OP_NAME`) and best-effort otherwise — an unknown opcode is
`OP_UNKNOWN`, a truncated push `[error]`. Pass `decode_sighash = true` for a
`scriptSig`, so a DER signature's trailing sighash byte is decoded as `[ALL]` /
`[NONE]` / …; pass `false` for a `scriptPubKey`, which never carries signatures.

This is **narrower** than `zaino-primitives`' `classify_script`, which keys
*every* script (non-standard ones included) for indexing and reads a 21-byte
`tag || hash` form. The two answer different questions: `classify_script`
produces an index key for any output; this produces a verifiable address or
nothing. Do not swap one for the other.

## `ismine`

the legacy full node's `ismine` field reports whether the *node's wallet* holds
the key. Zaino has no wallet, so the answer is always `false`. `validateaddress`
omits it (it is not a wallet concern there); `z_validateaddress` emits
`ismine: false` on a valid address, because zcashd and zebra both do and the
NightHawk explorer's `z_validateaddress` wire-parity requires the key. The byte
shape is the serving layer's: see `zaino-noderpc`'s `z_validated_to_wire`.

## Deprecation

`z_validateaddress` is deprecated upstream. `DEPRECATION_NOTICE` is exported for
the serving layer to log on every call; `validateaddress` is not deprecated and
carries no notice.

## Related

- ADR-0009 — why the serde impls live in `zaino-serve` rather than here.
