# Testing

Zaino has two kinds of test. The unit and model tests in `packages/*` run on the host
with `cargo nextest run --workspace` and need no validator, although they are not
network-free (`zaino-grpc`, for example, binds a loopback socket). The live tests in
`live-tests/` run a deployed zainod against a real zebrad on Kubernetes, driven by
[ztest](https://github.com/zingolabs/ztest).

| Set                              | What it covers                                                                                                                                                                                                                        |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `live-tests/clientless`          | zainod's gRPC answers checked against the validator as oracle, with no wallet.                                                                                                                                                        |
| `live-tests/e2e`                 | A real wallet syncing and sending through zainod's gRPC to a live validator.                                                                                                                                                          |
| `live-tests/non-finalized-state` | Reorgs forced on a regtest zebrad and checked against the validator across every index, the served tip, restarts and the window edges. Its contracts are in its [README](../live-tests/non-finalized-state/README.md). |
| `live-tests/sync`                | The long-running mainnet profiles `zaino_index_construction` and `zaino_index_crash_consistency`.                                                                                                                                     |

`live-tests/` is a standalone Cargo workspace, and `live-tests/sync/` is a second one
nested inside it, each with its own lock. The rules for writing live tests are in
[live-tests/CLAUDE.md](../live-tests/CLAUDE.md).

## Durability and model tests

Beyond per-function unit tests, these tests prove what the on-disk indexes and the
sync pipeline guarantee (the crash contract is in the
[`zaino-persistence` guide](../packages/zaino-persistence/usage.md)). Each is
checked against an independent oracle.

| Test                                                                                                   | Oracle                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `every_crash_state_*` (persistence and each index)                                                     | `SimFs` enumerates every state a power loss at each persistence point could leave. Each must reopen to an acknowledged or attempted commit and keep committing.                                                                                                                                                                                                                                                          |
| `random_histories_*` (compact-block, tree-state, transparent-address)                                  | Random finalized streams (bulk runs, folded tip steps, restarts resending held heights) are checked against a naive model: summed tree sizes for compact blocks, `incrementalmerkletree` frontiers for tree state, and a recomputed UTXO set for transparent addresses.                                                                                                                                                      |
| `the_tip_is_the_verified_best_and_its_holders_are_the_validators_asked_holding_it` (`zaino-chainview`) | Simulated zebrads mine, relay (longest chain wins, first seen on a tie, as on regtest), fork and go unreachable, and are polled in random order into a real header chain. After every poll the tip must be the header chain's best, its holders the validators whose last-polled chain holds it (no holder = no mempool), and each trusted validator's own tip its polled chain's (none once `Down`). |
| `random_header_trees_answer_like_the_naive_model` (`zaino-header-chain`)                               | Random trusted header trees (forks, finality, the kept finalized headers window) against a naive best-chain and finality model.                                                                                                                                                                                                                                                                                                     |
| `NfsCore` model (`zaino-nfs`, `nfs_core/model_tests.rs`)                                               | Random verified-chain evolutions (extensions, reorgs at random depth, same-height replacements, retreats, finality, restarts), late, stale and out-of-order bodies, delayed folds, naive finalized-path writers committing after random delays: every NFS publication must answer like folding each index from genesis along the best chain, and every case must settle with each index durable through the finalized tip and served at best. |
| `every_publication_answers_like_folding_from_genesis_*` (`zaino-nfs`)                                  | `Nfs` over the real finalized path (`FinalizedFollower` + every index writer, `SimFs` stores) through reorgs, finality and a restart, against each index folded from genesis into fresh stores.                                                                                                                                                                                                                            |
| `the_pipeline_follows_a_reorg_*` (`zainod`)                                                            | The whole pipeline over a mock validator and gRPC on localhost: `GetLatestBlock`, `GetBlockRange` and `GetTreeState` name one block before and after a reorg.                                                                                                                                                                                                                                                            |
| `committed_tree_states_are_zebras` (both sync profiles, every 5 s)                                     | On mainnet, `GetTreeState` at the tree-state index's durable tip must match zebrad's `z_gettreestate` byte for byte while the index builds, follows, and recovers from each kill.                                                                                                                                                                                                                                        |
| `committed_blocks_are_zebras` (both sync profiles, every 5 s)                                          | On mainnet, `GetBlock` at the compact-block index's durable tip must match the compact block implied by zebrad's `getblock 2`, field for field, with every fee recomputed from its prevouts. It runs while the index builds, so it samples blocks across the whole chain, and across each kill in the crash profile.                                                                                                     |
| `index_files_verify_clean` (both sync profiles, at completion)                                         | `zainod verify` runs in the pod and checks every durable byte of every index against its page checksums.                                                                                                                                                                                                                                                                                                               |

Every in-repo test takes its blocks, headers, verified chains and validators from one builder,
`zaino_source::testing::MockChain` ([guide](../packages/zaino-source/usage.md#testing-mockchain)):
every transaction is real consensus bytes written by librustzcash, every block it hands out is
the production decoder's reading of its own bytes and passes our own checks (linkage, merkle
root, spends, fees, pool activations), and its mempool holds only transactions its best chain
admits. The simulated validators are `zaino_source::testing::MockValidator`
(`chain.validator(tip)`), each following one tip of a shared `MockChain` and answering zebrad's
JSON-RPC as bytes. Tests reach them through Zaino's own client (`validator.client()`), so the
request builder, envelope, batching, parsers and error classification run in every test above
it. A test that relies on a request zebrad would refuse fails here rather than on a cluster.

### Model tests in bulk

Every model suite runs at its in-code case count under
`cargo nextest run --workspace`. To run them harder, use the `models` nextest
profile (`.config/nextest.toml`), which selects only the model suites and lifts the
default profile's 60 s per-test kill, and set the case count with `PROPTEST_CASES`,
which overrides the in-code value:

```sh
PROPTEST_CASES=5000 cargo nextest run --profile models
```

`PROPTEST_MAX_SHRINK_ITERS` shrinks a failure harder and `PROPTEST_VERBOSE=1`
prints each case. Raising `PROPTEST_CASES` across the whole workspace also scales
`transactions_decode_to_their_source` (`zaino-source`), which draws from
librustzcash's `arb_tx` and rejects ~80 draws per case: slow.

A new model suite joins the profile by its name: its test path (module included) contains
`model` or `random_histories` (`default-filter`). Its oracle is written
independently of the code under test, and moves the model cannot apply are
clamped rather than filtered out, so shrinking stays effective. A failing case
is saved and replays on every run, on every machine and in CI: beside an
integration test as `<test>.proptest-regressions`, or under the crate's
`proptest-regressions/` for an in-crate suite. Commit it with the fix.

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

`ztest run` MUST be launched from `live-tests/` and `ztest sync` from
`live-tests/sync/`, because neither finds its tests from any other directory.
`ztest run` takes `cargo nextest run` arguments, but its own flags such as `--rerun`
and `--cluster` MUST come before them.

## The `ztest` CLI

Both live workspaces depend on ztest by path, on a `ztest` checkout beside this
repository, so we install the CLI from that same checkout:

```sh
git clone https://github.com/zingolabs/ztest ../ztest   # from the repo root
cargo install --locked --path ../ztest/cli
```

The sync profiles also need a `zingolib` checkout beside `ztest`, and they build
zebrad's image from a `zebra` checkout beside this repository.

## Cluster setup

Any reachable Kubernetes cluster works. For a local kind cluster:

```sh
kind create cluster --name ztest
ztest cluster add kind --kind ztest --set-default   # profile "kind" on kind cluster "ztest"
ztest cluster setup                                 # provisions ztest's resources; takes a few minutes
ztest cluster check                                 # read-only readiness report
```

### Storage

The sync profiles restore validator chain-state snapshots, which are 40 to 200 GB on
mainnet. A CSI driver with copy-on-write `VolumeSnapshot` support makes that restore
fast, and without one the snapshot is copied. The other live tests need neither.
Pick the driver when you add the cluster profile:

```sh
ztest cluster add kind --kind ztest --storage-driver topolvm.io --set-default                    # LVM thin pools
ztest cluster add kind --kind ztest --storage-driver rook-ceph.cephfs.csi.ceph.com --set-default # shared cluster
```
