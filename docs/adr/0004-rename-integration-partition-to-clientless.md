# Rename the `integration` live-test partition to `clientless`; single `makers test` front door

## Status

accepted (supersedes the naming and task-surface decisions in ADR-0003)

## Context and decision

ADR-0003 kept the no-client live partition named `integration`, betting that the
`e2e` sibling disambiguated it. In practice the name still collided with Cargo's
vocabulary — every `tests/*.rs` is, to Cargo, an "integration test" — at every
grep, in the nextest filters (`package(integration)`), and in the CI binary-id
matrix (`integration::*`); "integration partition" also read ambiguously in
prose. We rename the partition everywhere it denotes that partition — crate,
directory, `-p` selector, nextest filters, CI matrix, glossary, and docs — to
**`clientless`**, which names the partition's actual distinguishing property (it
drives Zaino's service layer with **no wallet client**) and never collides with
Cargo's term. `integration` is thereby freed to mean only Cargo's sense.

We also collapse the three front-door tasks ADR-0003 introduced
(`offline-tests` / `live-tests` / `all-tests`, none of which shipped) into a
**single `makers test [SET]`** task, `SET ∈ {package, e2e, clientless, live,
all}`, default `package`. `live` = both live partitions (with the combined
summary); `all` = `package` then `live`. The set names mirror the crate/dir
names; the engines (`container-test`, `live-clientless`, `live-e2e`) are
unchanged.

## Considered options

- **Keep `integration` (ADR-0003's choice).** Rejected: the Cargo-vocabulary
  collision ADR-0003 tolerated is real and recurring (grep, nextest filters, CI
  `integration::*`). `clientless`/`e2e` is also a cleaner antonym pair than
  `integration`/`e2e`.
- **Term: `walletless` / `no-client` / `clientless`.** `walletless` was the
  pre-ADR-0003 name, but the e2e "client" is the `zcash_local_net` devtool
  client, not strictly a wallet. `clientless` is precise and pairs with `e2e`.
  (It had previously been listed under `_Avoid_`; that listing is reversed here.)
- **Three task front doors vs one.** ADR-0003's `offline-tests` /
  `live-tests` / `all-tests` were never released; one `makers test <set>` is a
  smaller, discoverable surface and keeps the set names aligned with the crate
  names.

## Consequences

- The crate/dir is `live-tests/clientless` (package `clientless`); selected via
  `-p clientless`, nextest `package(clientless)`, and CI `clientless::*`
  binary-id filters. `update-test-targets` regenerates the CI matrix from these.
- `integration` is now reserved for Cargo's sense; the `CONTEXT.md` `_Avoid_`
  lists are updated so "integration test" points at Cargo's meaning and
  "clientless" is no longer avoided.
- ADR-0003's naming bullet and its three-front-door task surface are superseded;
  its two-crate split and the `live` umbrella decision still stand.

> **Later note:** the `package` set was renamed `packages` (plural, matching the
> `packages/` dir); `makers test package` now errors. See `live-tests/CONTEXT.md`.
