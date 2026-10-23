# Updating Zebra

Zaino's validator path links no Zebra crate. `zaino-source` fetches each block as consensus
bytes (JSON-RPC `getblock <hash> 0`, or the indexer gRPC), decodes it with librustzcash, and
parses every other JSON-RPC result itself (`packages/zaino-source/src/json.rs`). The one Zebra
link is `zaino-peers`, which embeds `zebra-network` (and `zebra-chain`) as Zaino's p2p layer,
pinned by git rev in the root `Cargo.toml`.

So keeping up with Zebra means testing against each new zebrad release, plus moving the
`zebra-*` rev when the p2p layer needs it. A spread between the latest zebrad and the one the
live suite tests is high-priority tech debt. When checking a JSON-RPC response's shape, read the
tagged Zebra source for the node version you target, not the crates.io crates.

## Moving to a new zebrad

1. Take a baseline on `dev` with `cargo nextest run --workspace` and the live suite.
1. Bump the version in every `Validator::zebrad("…")` under `live-tests/`, and the zebrad
   version stated in [running.md](./running.md#requirements).
1. Check out the matching tag in the `zebra` checkout beside this repository: the sync profiles
   build zebrad's image from it.
1. Run the live tests ([testing.md](./testing.md)). A changed response shape shows up as a parse
   error in `zaino-source`, which is where it is fixed.

## Pinning unreleased dependencies

librustzcash's crates (`zcash_primitives`, `zcash_protocol`, `zcash_transparent`,
`zcash_address`, `zcash_encoding`, `equihash`) and the crates under them (`orchard`,
`sapling-crypto`, `incrementalmerkletree`) track upstream closely. An unpublished change is
pinned through the root `[patch.crates-io]` (Cargo honours it only in the workspace root), by
git rev, or by path to a sibling checkout while a change spans several repositories. Patching
one of these crates usually forces patching the others, so every crate agrees on one version of
the types that cross between them; the `zebra-*` dependencies point at a zebra fork built on the
same forks for the same reason. Comment each patch with why it is not a published version and
when to drop it.
