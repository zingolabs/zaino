# Testing

Zaino has two kinds of test. The unit and model tests in `packages/*` run on the host
with `cargo nextest run --workspace` and need no validator, although they are not
network-free (`zaino-grpc`, for example, binds a loopback socket). The live tests in
`live-tests/` run a deployed zainod against a real zebrad on Kubernetes, driven by
[ztest](https://github.com/zingolabs/ztest).

| Set | What it covers |
|---|---|
| `live-tests/clientless` | zainod's gRPC answers checked against the validator as oracle, with no wallet. |
| `live-tests/e2e` | A real wallet syncing and sending through zainod's gRPC to a live validator. |
| `live-tests/non-finalized-state` | Reorgs forced on a regtest zebrad and checked against the validator across every index, the serving gate, restarts and the window edges. The design is in [non-finalized-state-tests.md](design/non-finalized-state-tests.md). |
| `live-tests/sync` | The long-running mainnet profiles `zaino_index_construction` and `zaino_index_crash_consistency`. |

`live-tests/` is a standalone Cargo workspace, and `live-tests/sync/` is a second one
nested inside it, each with its own lock. The rules for writing live tests are in
[live-tests/CLAUDE.md](../live-tests/CLAUDE.md).

## Durability and model tests

Beyond per-function unit tests, these tests prove what the on-disk indexes and the
sync pipeline guarantee (see [durability.md](./design/durability.md) §7). Each is
checked against an independent oracle.

| Test | Oracle |
|---|---|
| `every_crash_state_*` (persistence and each index) | `SimFs` enumerates every state a power loss at each persistence point could leave. Each must reopen to an acknowledged or attempted commit and keep committing. |
| `random_histories_*` (each index) | Random apply, finalize, reset and reopen sequences are checked against a naive model: summed tree sizes for compact blocks, `incrementalmerkletree` frontiers for tree state, and a recomputed UTXO set for transparent addresses. |
| `followers_track_the_best_chain_through_random_reorgs` (`zaino-sync`) | A producer and follower run against a validator whose best chain moves at random, up to the window depth. |
| `committed_tree_states_are_zebras` (`zaino_index_construction`, every 5 s) | On mainnet, `GetTreeState` at the tree-state index's durable tip must match zebrad's `z_gettreestate` byte for byte while the index builds and follows. |
| `index_files_verify_clean` (both sync profiles, at completion) | `zainod verify` runs in the pod and checks every committed byte of every index against its page checksums. |

Two pieces are not built yet:

- TODO: `cargo-fuzz` targets for every decoder (`Entry`, height records, subtree
  entries, run rows, manifest bodies, and `project`/`framed_len` against a prost
  decode) and for each store's `open` over arbitrary file images. The target must never
  panic or SIGBUS, and must end in either an error or a state that `verify` reports
  clean.
- TODO: a nightly run of the crash scenarios on a
  [LazyFS](https://github.com/dsrhaslab/lazyfs) mount (`clear-cache`, `torn-op`,
  `torn-seq`), to check the `SimFs` model against real syscalls.

## Commands

```sh
cargo nextest run --workspace              # from the repo root

cd live-tests
ztest run                                  # clientless, e2e and non-finalized-state
ztest run -p clientless                    # one partition
ztest run --rerun latest -p clientless     # the last run's failures

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
