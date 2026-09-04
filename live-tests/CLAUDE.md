# Live-test Contributor Guidelines

The repo-root `CLAUDE.md` applies here in full. This file adds the rules that
are specific to the live suite, and overrides the root where it says so.

## What this tree is

Two test crates over a shared helper crate:

- `e2e` — a wallet drives Zaino over gRPC/JSON-RPC.
- `clientless` — no wallet; tests gRPC driectly
- `zaino-testutils` — Test helpers and utilities

`live-tests/` is a standalone Cargo workspace with its own lock. All the tests
in live-tests/ should be ztest-based and run inside the containerized zcash stack.
Any tests that need to validate zaino internal APIs and dont need a real validator
or wallet should be unit tests within the `packages/` directory.

## Running

Cluster setup and the `ztest` CLI install are in
[`docs/testing.md`](../docs/testing.md).

- launch `ztest run` from within the `live-tests/` directory
- Most cargo nextest options work, like `-p`, `-E`, `--rerun latest`
- If you need to inspect the container after a test terminates, use
  `ztest run --no-cleanup` and then `kubectl get pods` to see the cluster state

Never `#[ignore]` a test that can run on the cluster. If a test is blocked,
write the **full body** and let it fail on-cluster; a red test is information, a
skipped one is not.

## Every test owns its topology, inline

Setup is written out in the test body. Do not hide it behind a helper in another
crate that takes eight booleans — that pattern is much harder to read. We DO NOT
want to use DRY principles within integration tests. We also should not put any
comments in test body. If we do need a comment, it MUST be 1-2 lines, and
informative

```rust
let mut env = TestEnv::builder().ready_timeout(READY);
let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
env.build().await?;
```

## Parameterize the axes, don't copy the test

Two tests differing only by backend, validator, or pool are one `#[rstest]`.

```rust
#[rstest]
#[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
#[case::state(Validator::zebrad("6.2.3"), Backend::State)]
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn block_count<B: ValidatorConfig>(
    #[case] validator: Validator<B>,
    #[case] backend: Backend,
) -> Result<()> {
```

Zebra is the only supported backing validator, so the live axes are the ingest
path (`Fetch` / `State`) and the pool. `State` reads the validator's on-disk
zebra DB over a shared volume, so it exists only where that volume is mounted —
the axes are one flat case list, not a matrix.

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
