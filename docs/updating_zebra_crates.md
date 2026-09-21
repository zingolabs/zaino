# Updating Zebra crates

Zaino indexes from Zebra, and keeping its Zebra dependency close to the latest
release is a priority for Zaino maintainers: a small delta means no surprises
when new versions are released. A spread between the latest and the supported
version is treated as high-priority tech debt.

## What depends on Zebra

Zaino depends on a single Zebra crate, `zebra-chain`, used only by
`zaino-source-zebra-rpc` (block and transaction parsing for blocks fetched over
Zebra's JSON-RPC). Every other crate uses zaino's own domain types
(`zaino-primitives`), so a Zebra update is confined to that adapter crate.
Confirm with `cargo tree -i zebra-chain`.

Behaviour of the running node is decided by the `zebrad` release, not the
vendored crate; when checking a JSON-RPC response shape, read the tagged Zebra
source for the node version you target.

## How to update

We expect readers to be familiar with the [testing](./testing.md)
documentation.

1. Establish a baseline on `dev`: `cargo nextest run --workspace --all-features`.
2. Bump `zebra-chain` once, in the root `Cargo.toml`
   `[workspace.dependencies]`; members inherit it via `workspace = true`.
3. Fix `zaino-source-zebra-rpc`, then re-run the unit tests and the live tests
   in `live-tests/` (via ztest) against the matching `zebrad` image.

## Pinning to an unreleased zebra (git rev)

When Zaino needs a zebra change not yet published to crates.io, add a root
`[patch.crates-io]` entry pointing `zebra-chain` at a specific
`ZcashFoundation/zebra.git` rev. Cargo honours `[patch.crates-io]` only in the
workspace root, so member manifests must not carry their own patch sections.

Add an inline comment at the patch site explaining why the pin is a git rev,
and reference a tracking issue to revert to a published version once the
upstream change is released.

## Updating librustzcash dependencies

Stick with the latest tag you can find. `zebra-chain` depends on librustzcash
crates, so a zebra update can force a librustzcash update. Find the highest
common version across the zebra-pinned librustzcash crates on a per-crate basis.
