# Live-test Contributor Guidelines

The repo-root `CLAUDE.md` applies here in full. This file adds the rules that
are specific to the live suite, and overrides the root where it says so.

## What this tree is

- `e2e` — a wallet drives Zaino over gRPC.
- `clientless` — no wallet; tests call Zaino's gRPC directly.
- `non-finalized-state` — reorgs, the serving gate and every tip-dependent RPC
  ([design](../docs/design/non-finalized-state-tests.md)). All reorg testing
  lives here; sync profiles only tolerate reorgs.
- `zaino-testutils` — shared test helpers.
- `sync/` — long-running sync profiles over snapshot-restored public chains. A
  **separate** workspace with its own lock.

`live-tests/` is a standalone Cargo workspace with its own lock. Every test here
is ztest-based and runs against a containerized zebrad + zainod stack. A test
that exercises Zaino internals and needs no real validator or wallet is a unit
test in `packages/`, not a live test.

## Running

Cluster setup and the `ztest` CLI install are in
[`docs/testing.md`](../docs/testing.md).

- `ztest run` from `live-tests/`; most nextest options work (`-p`, `-E`,
  `--rerun latest`)
- `ztest sync` from `live-tests/sync/`
- `ztest run --no-cleanup`, then `kubectl get pods`, to inspect pods after a test

Never `#[ignore]` a test that can run on the cluster. If a test is blocked,
write the **full body** and let it fail on-cluster; a red test is information, a
skipped one is not.

## Every test owns its topology, inline

Setup is written out in the test body, never behind a helper in another crate
that takes eight booleans. No DRY within live tests, including no shared
assertion helpers. No comments in a test body; one that earns its place is 1-2
lines and informative.

```rust
let mut env = TestEnv::builder().ready_timeout(READY);
let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
env.build().await?;
```

## Parameterize the axes, don't copy the test

Two tests differing only in one input (typically the pool) are one `#[rstest]`.

```rust
#[rstest]
#[case::sapling(Pool::Sapling)]
#[case::ironwood(Pool::Ironwood)]
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn send_to_self(#[case] pool: Pool) -> Result<()> {
```

Zebra is the only validator and Zaino has one ingest path (zebrad JSON-RPC), so
neither is an axis. A single-case `#[rstest]` is noise: write a plain test.

## Tag the QoS tier, and tag it honestly

Every test carries exactly one `#[ztest::qos::*]`. The tier is a resource
reservation; getting it wrong OOM-kills pods and reads as flake.

| Tier          | Reserve      | Cap    | Use                                     |
| ------------- | ------------ | ------ | --------------------------------------- |
| `basic`       | 1c / 512 MiB | 1 min  | No validator pod.                       |
| `wallet`      | 4c / 2 GiB   | 10 min | In-process wallet; proving work.        |
| `integration` | 3c / 3 GiB   | 10 min | ≤3-pod zaino topology, no wallet.       |
| `testnet`     | 8c / 10 GiB  | 6 h    | Snapshot-restored public chains.        |
| `sync`        | 15c / 15 GiB | 48 h   | Long-running sync subjects (NVMe pool). |

A regtest zebrad needs 1 GiB to itself — `basic` will kill it. Anything standing
up a validator is `integration` or heavier.

## Runtime attributes

The root `CLAUDE.md` rule ("start at `#[test]`, escalate only as the body
demands") does **not** apply here. Every live test drives pods over the network
and awaits concurrent readiness, so `#[tokio::test(flavor = "multi_thread")]` is
the floor. Do not downgrade one, and do not add a justifying comment — it is the
default for the whole tree.

## Comments

Follow ztest's comment discipline, which is stricter than the root file's:
notes, not prose; no restating the code; no provenance trivia.

Specifically banned in this tree, because it is where the suite keeps regrowing
them:

- **Migration archaeology.** "Port of `x`", "upstream did", "dev drove", "used
  to live here". The commit message is where that goes.
- **Tombstones for deleted tests.** If a test is gone, it is gone. A comment
  explaining an absence is unfalsifiable and never gets deleted.
- **Doc comments echoing the test name.** `/// Tests that the block count matches` above `async fn block_count` is zero information. Write what the test
  *asserts* that the name does not say, or write nothing.
