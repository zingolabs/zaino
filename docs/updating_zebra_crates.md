# Updating Zebra

Zaino's validator path links no Zebra crate. `zaino-source` fetches each block by
hash as consensus bytes (`getblock <hash> 0`), splits the header with
`zaino_primitives::types::HeaderBytes` and decodes the transactions with
librustzcash (`zcash_primitives`'s `CompressedTransaction`), and it parses every
other JSON-RPC result into `zaino-primitives` types in
`packages/zaino-source/src/parse.rs`. The one Zebra link is `zaino-peers`, which
embeds `zebra-network` (and `zebra-chain`) as Zaino's p2p layer, pinned by git rev
in the root `Cargo.toml` (`cargo tree -i zebra-chain` shows only it). Zaino's
coupling to the node is therefore the JSON-RPC surface of the zebrad release it runs
against, and keeping up with Zebra means testing against new zebrad releases, plus
moving the `zebra-*` rev when the p2p layer needs it. We treat a spread between the
latest zebrad and the one the live suite tests as high-priority tech debt.

Node behaviour is decided by the `zebrad` release. When checking the shape of a
JSON-RPC response, read the tagged Zebra source for the node version you target.

## Moving to a new zebrad

1. Take a baseline on `dev` with `cargo nextest run --workspace` and the live suite.
1. Bump the version in every `Validator::zebrad("6.2.3")` under `live-tests/`, and
   the zebrad version stated in [running.md](./running.md#requirements).
1. Check out the matching tag in the `zebra` checkout beside this repository, since
   the sync profiles build zebrad's image from it.
1. Run the live tests (see [testing.md](./testing.md)). A changed response shape
   shows up as a parse error in `zaino-source`, which is where it is fixed.

## Pinning unreleased dependencies

The crates that do track upstream closely are librustzcash's (`zcash_primitives`,
`zcash_protocol`, `zcash_transparent`, `zcash_address`, `zcash_encoding`,
`equihash`) and the crates under them (`orchard`, `sapling-crypto`,
`incrementalmerkletree`).
We pin an unpublished change through the root `[patch.crates-io]`, by git rev, or by
path to a sibling checkout while a change spans several repositories. Cargo honours
`[patch.crates-io]` only in the workspace root. Patching one of these crates usually
forces patching the others with it, so that every crate agrees on one version of the
types that cross between them. Comment each patch with why it is not a published
version and when to drop it.

The current patch set is the lazy point-decompression stack. It pins `orchard`,
`sapling-crypto`, `incrementalmerkletree` and the librustzcash crates to forks by git
rev; the `zebra-*` workspace dependencies point at a zebra fork built on the same
forks, so both sides agree on one version of each type. Each entry is dropped once
its change is released.
