# Live tests rejoin the root workspace under a single lock

## Status

accepted

## Context and decision

The integration tests were split into two standalone Cargo workspaces —
`integration-tests/` (walletless) and `integration-tests/wallet-tests/` —
primarily to isolate **zingolib**'s conflicting dependency resolution
(orchard / zebra / `time`), with both lockfiles **gitignored**. zingolib has
since been fully removed from the tree (no occurrences in `Cargo.lock`), so the
isolation no longer buys anything.

We therefore reintegrate the live-test crates (`e2e`, `integration`,
`zaino-testutils`) as **members of the root workspace**, and keep them out of
the fast development loop with **`default-members`**:

- `members` gains `live-tests/{e2e, integration, zaino-testutils}`.
- `default-members = [packages/*]` (the six production crates only).
- `container-test` runs a bare `cargo nextest run` (no `--workspace`, no `-p`),
  so it operates on `default-members` and **never builds or tests the live
  suite**; the production crates don't depend on `zcash_local_net`, so the git
  dep isn't even fetched in the dev loop. The live suite is reached explicitly
  via `cargo nextest run -p e2e` / `-p integration` (the `live` task family).
- A **single committed root `Cargo.lock`** now resolves the entire graph,
  including the `zcash_local_net` git dependency (pinned by rev) and the
  zebra/zcash test stack. The two gitignored integration lockfiles are deleted.

## Considered options

- **Keep separate workspaces (status quo).** Isolates the production lock from
  the test graph, but duplicates dependency resolution, *requires* gitignored
  locks (so each machine re-resolves a moving git branch — the source of the
  `time` 0.3.x drift flakiness), and prevents shared compilation across the two
  partitions.
- **Use `optional` / `dev-dependencies` to keep the test graph out of the
  production lock.** Impossible: a workspace `Cargo.lock` is the *maximal*
  resolution over **all** members, including every `dev-dependency` and
  feature-gated `optional` dependency. The only way to keep a dependency graph
  out of a lock is for the crate not to be a member — i.e. the status quo we are
  deliberately ending.

## Consequences

- The production lockfile carries the test graph and a git-sourced dependency.
  This is **inert for production builds** (`cargo build -p zainod` still
  resolves only zainod's own graph) and **temporary for the git dep**
  (zingolabs/infrastructure#269 will restore a published `zcash_local_net`
  release, after which the lock shows an ordinary registry dependency).
  `cargo deny` / audit now also scan the test dependencies — arguably a feature.
- The single committed lock **fixes** the `time`-drift flakiness by pinning one
  resolution for every machine. Add an explicit `time = "=<good>"` to root
  `[workspace.dependencies]` only if a future resolution lands on a broken
  version.
- `[profile.test]` is hoisted to the root workspace (it only takes effect from
  the workspace root anyway), and the shared dependency stack compiles **once**
  when the two partitions request the same feature set — which the default
  feature build does.
- The vestigial `exclude = ["zaino-testutils"]` entry (a path that never
  existed at the root) is removed along with `exclude = ["integration-tests"]`.
