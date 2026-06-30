# Live-test taxonomy and two-crate split

## Status

accepted — naming (`integration` → `clientless`) and the three-front-door task
surface are superseded by [ADR-0004](0004-rename-integration-partition-to-clientless.md);
the two-crate split and the `live` umbrella still stand.

## Context and decision

With zingolib removed, the two test packages are reunified — and renamed, because
"integration-tests" both **collides with Cargo's own vocabulary** (every
`tests/*.rs` is, to Cargo, an "integration test") and **undersells** what the
suite does: it stands up real external processes and exercises the assembled,
running system.

We adopt this taxonomy, captured as ubiquitous language in the suite's
`CONTEXT.md`:

- **`live`** — the umbrella (directory `live-tests/`). The defining property of
  the suite is that it requires a **live validator** process (Zebra or zcashd),
  and optionally a live wallet client. That property — not "it has a `tests/`
  folder" — is what separates it from the inline unit tests `container-test`
  runs, and is the reason it is excluded from the default flow.
- **`e2e`** (was `wallet-tests`) — the partition driven end-to-end by a real
  wallet client through Zaino's gRPC surface to a live validator.
- **`integration`** (was `walletless-tests`) — the partition that drives Zaino's
  service layer (`FetchService` / `StateService`, RPC surface) directly against
  a live validator, with no wallet client.

The two partitions are **separate member crates**, not sibling modules of one
package. `zaino-testutils` remains a separate, client-agnostic shared crate that
both depend on.

## Considered options

- **Naming.** `integration` (status quo) was rejected as overloaded and
  uninformative — and it collides with Cargo's term. `e2e` as the *umbrella* was
  rejected as overclaiming: roughly half the tests (the `integration` partition)
  have no client "end" and don't cross the gRPC boundary. `system` was rejected
  for faint redundancy with the `e2e` partition and a black-box/acceptance
  connotation these developer-facing differential tests don't fit. `live` won
  because it names the literal distinguishing property (needs a live validator)
  with zero redundancy.
- **One package with `e2e` / `integration` sibling mods** (the initial plan) vs.
  **two member crates.** Two crates won: the partitions have **zero
  cross-references** (verified — nothing in `e2e` touches `walletless_tests`,
  nothing in `integration` touches `wallet_tests`) and everything genuinely
  shared already lives in `zaino-testutils`, so splitting duplicates **no code**.
  Two crates also yield **disjoint, precise feature tables** (`devtool-incompatible`
  is e2e-only; `experimental_features` / `transparent_address_history_experimental`
  is integration-only) instead of a muddier merged union, and match the natural
  `live-tests/{e2e, integration}` layout without a redundant `live/live` nesting.

## Consequences

- A crate **literally named `integration`** sits inside `live-tests/`. This is
  intentional: at the leaf it precisely denotes the no-client partition, and the
  `e2e` sibling disambiguates it — the overload that sank "integration" as the
  *umbrella* is resolved by the contrast.
- The developer task surface is three front doors — `offline-tests` (the
  `packages/*` set needing no live validator), `live-tests` (both live
  partitions, aggregated), and `all-tests` (both) — delegating to the engines
  `container-test`, `live-integration`, and `live-e2e` (partition selection
  `-p e2e` / `-p integration`). "offline" is the glossary contrast to "live"
  (see `live-tests/CONTEXT.md`).
- Per-crate manifest boilerplate is the only duplication, and it is minimized by
  `[workspace.package]` inheritance, `[workspace.dependencies]`, a root
  `[profile.test]`, and a single root `.config/nextest.toml`.
- The `e2e` partition is **compiled but not executed in CI**: it is part of the
  `cargo nextest archive --workspace` build (so compilation regressions are
  caught), but it is absent from the CI test matrix (the per-partition
  `binary_id` selection in `ci.yml` simply never names an `e2e` binary). Adding
  the validator-heavy e2e suite to CI is a
  capacity decision deferred to
  [#1308](https://github.com/zingolabs/zaino/issues/1308); until then, e2e
  *runtime* regressions are ungated. This is a logged trade-off, not an oversight.
