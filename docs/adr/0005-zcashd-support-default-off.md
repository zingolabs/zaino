# `zcashd_support` is opt-in, not a default feature

## Status

accepted (supersedes the default-on decision in ADR-0001)

## Context and decision

ADR-0001 made `zcashd_support` a **default-on** additive feature, with the test
harness opting out via `--no-default-features`. That left zcashd enabled by
default on every path *outside* the harness — bare `cargo build` / `cargo
nextest run`, and any consumer of the crates — and it required a fragile
`default-features = false` annotation on every internal `zaino-*` path
dependency to drop the feature end-to-end. The zcashd-backed tests could also be
switched on implicitly through an ambient `CONTAINER_TEST_WITH_ZCASHD=1` env var.

We make `zcashd_support` **opt-in**: `default = []` in all crates that defined
it, the feature definitions (`zcashd_support = [...]`) unchanged. Nothing enables
zcashd unless explicitly asked. Concretely:

- Bare `cargo build` / `cargo nextest run` now compile zcashd out by default.
- The internal `zaino-*` path deps no longer carry `default-features = false`
  (their default set is already empty) — the annotation and its rationale are
  removed.
- The test harness enables zcashd only via the explicit `--with-zcashd` flag,
  which adds `--features zcashd_support`. The `CONTAINER_TEST_WITH_ZCASHD` env
  var is **retired**; the flag is forwarded through the task chain instead, so
  there is no implicit/ambient enable.

## Considered options

- **Keep ADR-0001's default-on.** Rejected: zcashd-on-by-default is exactly the
  implicit behaviour we want gone during deprecation, and the consumer-facing
  default should reflect the zebrad-only future.
- **Flip the default but keep the env var.** Rejected: an exported
  `CONTAINER_TEST_WITH_ZCASHD=1` is a sticky implicit enable; collapsing to a
  single explicit flag is simpler and matches the "never implicit" goal.

## Consequences

- `--no-default-features` is now a no-op for the flipped crates (their default
  is empty); CI and the target-list scripts keep passing it (still correct: it
  also drops other crates' defaults, e.g. `zaino-proto`'s `heavy`).
- Enabling zcashd is `--features zcashd_support` (cargo accepts it at the
  virtual workspace root and with `-p`), surfaced as `--with-zcashd` /
  `makers zcashd_test`.
- ADR-0001's feature, semantics, name, and gated scope still stand; only its
  default membership changed.
