# Updating Zebra crates ~best~ possible practices.

Zaino depends on Zebra as its main validator. Zainod and Zebrad are
tightly coupled. Keeping up-to-date with latest Zebra crates is
a priority for Zaino maintainers. A lesser delta between the
zebra-crates Zaino depends on and their latest ensures that there
are no surprises when new versions of these crates are released.

When there's a spread between latest and supported version of the
Zebra crates we consider that a high priority _tech debt_.

# How to approach updating Zebra crates

Note: We expect readers of this document are familiarized with the [testing](./testing.md)
documentation. If you haven't done so, please familiarize yourselve with that
document first

## Pre-condition: run all test and establish a baseline
Run all tests on `dev` with `cargo nextest run --all-features`

This baseline will tell you which tests are currently passing, failing
and their performance. This will help you identify regressions when
updating these or any other dependencies.

## update `.env.testing-artifacts` to the corresponding version of Zebra
Instructions on how to do this can be found in [testing](./testing.md)
documentation.

## Finding out which crates depend on Zebra crates.
Find out which dependencies use `zebra-*` crates by running
`cargo tree` and spotting the usage of Zebra crates.

## Always specify `all-features` when building

Make sure you build and run the project with `all-features` in
order to catch any posible compile errors early.

## Keep the zebra pin in sync across all three workspaces

This repo is **three separate Cargo workspaces**, each with its own
`Cargo.lock`:

- `Cargo.toml` — the root/production workspace (`packages/*`).
- `integration-tests/Cargo.toml` — the walletless-tests workspace.
- `integration-tests/wallet-tests/Cargo.toml` — the wallet-tests workspace.

The production crates (notably `zaino-state`) are consumed by both
integration-test workspaces as **path dependencies**. A path dependency
is compiled against the **host workspace's** dependency resolution — not
the root's. So a `[patch.crates-io]` (or version bump) applied only to the
root `Cargo.toml` does **not** reach `zaino-state` when it is built inside
an integration-test workspace; that workspace resolves whatever the plain
version requirement points at (a crates.io release).

Consequence: any change to the zebra pin — a version bump, or pinning to a
git rev — **must be applied identically in all three manifests**. If they
drift, the same `zaino-state` source compiles against two different zebra
versions and you get errors like `E0559` (a field/variant that exists on
one zebra version but not the other), typically surfacing first in a
container test run rather than in `cargo check` at the root.

## Pinning to an unreleased zebra (git rev)

Sometimes Zaino needs a zebra change that has not yet been published to
crates.io (for example, a new field on a `ReadRequest` variant). In that
case the `[patch.crates-io]` entries point `zebra-chain` / `zebra-rpc` /
`zebra-state` at a specific `ZcashFoundation/zebra.git` rev instead of a
published version.

When you do this:

1. Mirror the **exact same** patch block into all three workspace
   manifests (see the section above) — the git source carries a Cargo
   version that can equal a published version while differing in content
   (e.g. an unreleased `9.0.1` that is not the crates.io `9.0.1`).
2. Add an inline comment at each patch site explaining *why* the pin is a
   git rev, and reference a tracking issue to revert to a plain version
   once the upstream change is released. Pinning to an unreleased rev is
   tech debt; it should not outlive the release that obsoletes it.

## Juggling transitive dependencies
### Tonic
Tonic is used in Zebra, Zaino and Librustzcash. This one is
going to be a challenge. Priotize what works with Zebra and then work
your way down the stack. Tonic can break the `.proto` files downstream if
you notice that there are significant issues consult with Zebra and
[Lightclient Protocol](https://github.com/zcash/lightwallet-protocol) maintainers.

### Prost
Prost is used in conjunction with `tonic` to build gRPC .rs files from `.proto` files
it is also used accross many crates like `zaino-proto` and `zebra-rpc`. Zaino can't build
without reliably generating the files so it's
important to figure this dependency graph out.

## Updating Librustzcash dependencies.
Always try to stick with the latest tag you can find. Zebra uses Librustzcash
as well, so a zebra update can force a librustzcash update. Find the highest
common denominator across the zebra-pinned librustzcash crates on a per-crate
basis.
