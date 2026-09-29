# Testing

| Set | Where it runs | What it covers |
|---|---|---|
| `packages/*` | host, `cargo nextest run --workspace` | The production crates. No validator, but not network-free (e.g. `zaino-grpc` binds a loopback socket). |
| `live-tests/clientless` | ztest / k8s | A deployed zainod against a live validator over gRPC, no wallet: validator-vs-Zaino oracle checks. |
| `live-tests/e2e` | ztest / k8s | A real wallet → Zaino gRPC → live validator. |
| `live-tests/non-finalized-state` | ztest / k8s | Reorgs forced on a regtest zebrad, checked against the validator as oracle, across every index, the serving gate, restarts and the window edges. Design: [non-finalized-state-tests.md](design/non-finalized-state-tests.md). |
| `live-tests/sync` | ztest / k8s | Long-running mainnet profiles: `zaino_index_construction`, `zaino_index_crash_consistency`. |

`live-tests/` is a standalone workspace, and `live-tests/sync/` is a second one
nested in it. Rules for writing live tests:
[live-tests/CLAUDE.md](../live-tests/CLAUDE.md).

## Durability and model tests

What the `packages/*` set proves about the on-disk indexes and sync, beyond
per-function unit tests ([durability.md](./design/durability.md) §7):

| Test | Oracle |
|---|---|
| `every_crash_state_*` (persistence, each index) | `SimFs` enumerates every state a power loss at each persistence point could leave; each reopens to an acknowledged or attempted commit and keeps committing |
| `random_histories_*` (each index) | Random apply / finalize / reset / reopen sequences against a naive model: summed tree sizes (compact), `incrementalmerkletree` frontiers (tree-state), a recomputed UTXO set (transparent) |
| `followers_track_the_best_chain_through_random_reorgs` (`zaino-sync`) | Producer + follower against a validator whose best chain moves at random, up to the window depth |
| `committed_tree_states_are_zebras` (`zaino_index_construction`, every 5 s) | Mainnet: `GetTreeState` at the tree-state index's durable tip against zebrad's `z_gettreestate`, byte for byte, while the index builds and follows |
| `index_files_verify_clean` (`zaino_index_construction`, at completion) | `zainod verify` in the pod: every committed byte of every index against its page checksums |

Not built yet:

- TODO: `cargo-fuzz` targets for every decoder (`Entry`, height records,
  subtree entries, run rows, manifest bodies, `project`/`framed_len` against a
  prost decode) and for each store's `open` over arbitrary file images: no
  panic, no SIGBUS, an error or a state `verify` reports clean.
- TODO: a nightly run of the crash scenarios on a
  [LazyFS](https://github.com/dsrhaslab/lazyfs) mount (`clear-cache`,
  `torn-op`, `torn-seq`), checking the `SimFs` model against real syscalls.

## Commands

```sh
cargo nextest run --workspace              # from the repo root

cd live-tests
ztest run                                  # clientless + e2e + non-finalized-state
ztest run -p clientless                    # one partition
ztest run -p clientless --rerun latest     # last run's failures

cd sync                                    # live-tests/sync
ztest sync start zaino_index_construction --watch
ztest sync start zaino_index_crash_consistency
```

Launch `ztest run` from `live-tests/` and `ztest sync` from `live-tests/sync/`;
from any other directory neither finds its tests.

## The `ztest` CLI

Both live workspaces depend on ztest by path, on a `ztest` checkout beside this
repository, so install the CLI from that checkout:

```sh
git clone https://github.com/zingolabs/ztest ../ztest   # from the repo root
cargo install --locked --path ../ztest/cli
```

The sync profiles also need `zingolib` beside `ztest`, and build zebrad from a
`zebra` checkout beside this repository.

## Cluster setup

```sh
kind create cluster --name ztest            # any reachable cluster works
ztest cluster add kind --kind ztest --set-default
ztest cluster setup                         # namespaces, monitoring; takes a few minutes
ztest cluster check                         # read-only readiness report
```

### Storage

Sync profiles restore validator chain-state snapshots (40–200 GB on mainnet).
A CSI driver with copy-on-write `VolumeSnapshot` support makes that fast;
without one the snapshot is copied. Integration runs need neither.

```sh
ztest cluster add kind --kind ztest --storage-driver topolvm.io --set-default                    # LVM thin pools
ztest cluster add kind --kind ztest --storage-driver rook-ceph.cephfs.csi.ceph.com --set-default # shared cluster
```
