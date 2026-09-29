# Updating Zebra crates

Keep the Zebra dependency close to the latest release; a spread between latest
and supported is high-priority tech debt.

## What depends on Zebra

One crate, `zebra-chain`, used only by `zaino-source-zebra-rpc` (block and
transaction parsing for blocks fetched over JSON-RPC). Every other crate speaks
`zaino-primitives`, so a Zebra update is confined to that adapter. Confirm with
`cargo tree -i zebra-chain`.

Node behaviour is decided by the `zebrad` release, not the `zebra-chain` crate:
when checking a JSON-RPC response shape, read the tagged Zebra source for the
node version you target.

## How to update

1. Baseline on `dev`: `cargo nextest run --workspace --all-features`.
2. Bump `zebra-chain` once, in the root `Cargo.toml` `[workspace.dependencies]`.
3. Fix `zaino-source-zebra-rpc`, then re-run the unit tests and the live tests
   (see [testing.md](./testing.md)) against the matching `zebrad` version.

`zebra-chain` depends on librustzcash crates, so a bump can force a librustzcash
bump: take the highest version each crate shares with the one Zebra pins.

## Pinning an unreleased Zebra

Pin an unpublished change through the root `[patch.crates-io]`: a git rev, or a
path to a sibling checkout while the change spans several repositories. Cargo
honours `[patch.crates-io]` only in the workspace root. Patching `zebra-chain`
usually forces patching the librustzcash crates with it, so that both sides
agree on one version of the types that cross between them. Comment each patch
with why it is not a published version and when to drop it.

The current patch set (the lazy point-decompression stack) pins `orchard`,
`sapling-crypto` and the librustzcash crates by git rev, and `zebra-chain` by
path to `../zebra`: a fresh clone does not build without that sibling checkout.
