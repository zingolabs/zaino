# Live-test Contributor Guidelines

The repo-root `CLAUDE.md` applies here in full. This file adds the rules that
are specific to the live suite, and overrides the root where it says so.

The sets, how to run them and the cluster setup are in
[`docs/testing.md`](../docs/testing.md).

## What belongs here

- A test that exercises Zaino internals and needs no real validator or wallet is a
  unit test in `packages/`, not a live test.
- All reorg testing lives in `non-finalized-state`
  ([README](non-finalized-state/README.md)); sync profiles only tolerate reorgs.

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

Zebra is the only validator, so it is not an axis (how Zaino reaches it:
[zainod: Validator client](../packages/zainod/usage.md#validator-client)). A single-case
`#[rstest]` is noise: write a plain test.

## Tag the QoS tier, and tag it honestly

Every test carries exactly one `#[ztest::qos::*]`. The tier is a resource
reservation; getting it wrong OOM-kills pods and reads as flake.

| Tier          | Use                                                                        |
| ------------- | -------------------------------------------------------------------------- |
| `integration` | Validator + zainod, no wallet (ztest's default).                           |
| `wallet`      | Any `env.add_wallet`: the in-process wallet runs in the test runner.       |
| `testnet`     | Snapshot-restored public chains.                                           |
| `sync`        | Long-running sync subjects (NVMe pool, `footprint` required).              |

Reserves and caps are ztest's (`QosClass::profile`, its `docs/design-qos.md`). A topology
other than validator + zainod declares a `footprint` rather than switching tier.

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
