# Testing

Zaino has two kinds of test. The unit and model tests in `packages/*` run on the host with
`cargo nextest run --workspace` and need no validator, although they are not network-free
(`zaino-grpc`, for example, binds a loopback socket). Their blocks, headers and validators come
from one builder, [`MockChain`](../packages/zaino-source/usage.md#testing-mockchain). The live
tests in `live-tests/` run a deployed zainod against a real zebrad on Kubernetes, driven by
[ztest](https://github.com/zingolabs/ztest).

| Set                              | What it covers                                                                     |
| -------------------------------- | ---------------------------------------------------------------------------------- |
| `live-tests/clientless`          | zainod's gRPC answers checked against the validator as oracle, with no wallet      |
| `live-tests/e2e`                 | a real wallet syncing and sending through zainod's gRPC to a live validator        |
| `live-tests/non-finalized-state` | reorgs forced on a regtest zebrad, checked across every index ([README](../live-tests/non-finalized-state/README.md)) |
| `live-tests/sync`                | the long-running mainnet profiles `zaino_index_construction` and `zaino_index_crash_consistency` |

`live-tests/` is a standalone Cargo workspace, and `live-tests/sync/` a second one nested inside
it, each with its own lock. The rules for writing live tests: [live-tests/CLAUDE.md](../live-tests/CLAUDE.md).

## Model tests in bulk

Every model suite runs at its in-code case count under `cargo nextest run --workspace`. To run
them harder, use the `models` nextest profile (`.config/nextest.toml`), which selects only the
model suites and lifts the default profile's 60 s per-test kill, and set `PROPTEST_CASES`:

```sh
PROPTEST_CASES=5000 cargo nextest run --profile models
```

`PROPTEST_MAX_SHRINK_ITERS` shrinks a failure harder and `PROPTEST_VERBOSE=1` prints each case.
Raising `PROPTEST_CASES` across the whole workspace also scales
`transactions_decode_to_their_source` (`zaino-source`), which is slow.

A new model suite joins the profile by its name: its test path contains `model` or
`random_histories`. Its oracle is written independently of the code under test, and moves the
model cannot apply are clamped rather than filtered out, so shrinking stays effective. A failing
case is saved and replays on every run: beside an integration test as
`<test>.proptest-regressions`, or under the crate's `proptest-regressions/`. Commit it with the fix.

### Heavy run

A change to `zaino-persistence`, `zaino-header-chain`, `zaino-nfs`, `zaino-grpc-snapshot` or
`zaino-traffic` is done only once this passes for that crate. Each round runs the crate's whole
suite on fresh seeds, so the timing-sensitive tests (background merges, failed I/O, crash
states) repeat alongside the models. Then the crates built on it run once:

```sh
crate=zaino-persistence
end=$((SECONDS + 180)); round=0
while [ $SECONDS -lt $end ]; do
  round=$((round + 1))
  PROPTEST_CASES=1000 cargo nextest run -p "$crate" --all-features --profile heavy \
    || { echo "$crate: FAILED in round $round"; break; }
  echo "$crate: round $round passed"
done

cargo nextest run --workspace -E "rdeps($crate)"   # after the loop passes
```

- `--profile heavy` lifts the default profile's 60 s kill (at 1000 cases a model test outlives
  it): a test still running at 300 s is reported slow, never killed.
- `--all-features` builds the crate with every optional feature on.
- Lengthen the run by raising `180`, never shorten it. Keep the test profile (no `--release`):
  overflow checks stay on.
- A failure saves its shrunk case (above). Fix the bug and rerun the whole block.

## Commands

```sh
cargo nextest run --workspace              # from the repo root

cd live-tests
ztest run                                  # clientless, e2e and non-finalized-state
ztest run -p clientless                    # one partition
ztest run --rerun latest -p clientless     # the last run's failures
ztest run --no-cleanup -p clientless       # keep each test's namespace (pods, logs) to inspect

cd sync                                    # now in live-tests/sync
ztest sync start zaino_index_construction --watch
ztest sync start zaino_index_crash_consistency
```

`ztest run` MUST be launched from `live-tests/` and `ztest sync` from `live-tests/sync/`: neither
finds its tests from any other directory. `ztest run` takes `cargo nextest run` arguments, but
its own flags such as `--rerun` and `--cluster` MUST come before them.

## The `ztest` CLI

Both live workspaces depend on ztest by path: `live-tests/` on a `ztest` checkout beside this
repository, `live-tests/sync/` on a `ztest-status` checkout beside it (each `Cargo.toml` names
the path). Install the CLI from the `ztest` checkout:

```sh
git clone https://github.com/zingolabs/ztest ../ztest   # from the repo root
cargo install --locked --path ../ztest/cli
```

The sync profiles also need a `zingolib` checkout beside `ztest-status`, and they build zebrad's
image from a `zebra` checkout beside this repository.

## Cluster setup

Any reachable Kubernetes cluster works. For a local kind cluster:

```sh
kind create cluster --name ztest
ztest cluster add kind --kind ztest --set-default   # profile "kind" on kind cluster "ztest"
ztest cluster setup                                 # provisions ztest's resources; takes a few minutes
ztest cluster check                                 # read-only readiness report
```

The sync profiles restore validator chain-state snapshots of 40 to 200 GB. A CSI driver with
copy-on-write `VolumeSnapshot` support makes that restore fast (without one the snapshot is
copied); the other live tests need neither. Pick the driver when you add the cluster profile:

```sh
ztest cluster add kind --kind ztest --storage-driver topolvm.io --set-default                    # LVM thin pools
ztest cluster add kind --kind ztest --storage-driver rook-ceph.cephfs.csi.ceph.com --set-default # shared cluster
```
