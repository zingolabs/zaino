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

## Keep the zebra pin in the single root workspace

This repo is **one Cargo workspace** with a single `Cargo.lock`: the live-test
crates were folded into the root workspace (see docs/adr/0002, docs/adr/0003).
The zebra version requirements live once in the root `Cargo.toml`
`[workspace.dependencies]`, and any unreleased pin lives once in the root
`[patch.crates-io]`. Every member — the `packages/*` production crates and the
`live-tests/{e2e,clientless,zaino-testutils}` crates alike — inherits them via
`zebra-* = { workspace = true }`.

Consequence: a zebra change — a version bump or a git-rev pin — is applied in
**one place** (the root `Cargo.toml`) and reaches every member through workspace
inheritance and the single lock. Cargo honours `[patch.crates-io]` only from the
workspace root, so member manifests must **not** carry their own patch sections
(they would be silently ignored).

(Historically this repo was three separate workspaces, each with its own lock,
so the pin had to be mirrored across all three manifests or the same
`zaino-state` source compiled against two zebra versions — `E0559`-style errors.
The reunification removed that footgun.)

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
