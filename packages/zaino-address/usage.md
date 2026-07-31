# `zaino-address` — usage

Zcash address classification. A leaf crate serving `validateaddress` and
`z_validateaddress`.

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
use zaino_address::{validate_address, z_validate_address};

let result = validate_address(address_string, network);
let result = z_validate_address(address_string, network);
```

Both return domain types (`ValidatedAddress`, `ZValidatedAddress`) with **no
serde**. The zcashd-shaped JSON — including the exact field sets, which differ
per address kind — is `zaino-serve`'s `wire/address.rs`, per ADR-0009.

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

## `ismine` is never emitted

zcashd's `ismine` field reports whether the *node's wallet* holds the key.
Zaino has no wallet, so it has no answer, and inventing `false` would be a claim
rather than an omission. `wire/address.rs` pins this with a test.

## Deprecation

`z_validateaddress` is deprecated upstream. `DEPRECATION_NOTICE` is exported for
the serving layer to log on every call; `validateaddress` is not deprecated and
carries no notice.

## Related

- ADR-0009 — why the serde impls live in `zaino-serve` rather than here.
