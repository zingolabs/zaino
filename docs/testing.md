# Testing

Zaino has two kinds of tests, run by two different commands:

- **Unit tests** — the unit and crate-level integration tests of the
  `packages/*` crates (the root workspace `default-members`). They need no live
  validator and run on your host with a plain `cargo nextest run`.
- **Integration/Live tests** — tests that stand up a real validator, wallet against
  regtest or testnet and exercise the assembled, running system.
  They live in the standalone `live-tests/` workspace and run on the **ztest**
  Kubernetes harness

## Quick start

```sh

# Production crates, from the repo root.
cargo nextest run

cd live-tests
ztest run

# Run just the clientless tests (no wallet)
ztest run -p clientless

# Run just the tests that failed last run
ztest run -p clientless --rerun latest

```

## The test sets

| Set          | Where it runs          | What it covers                                                                                                                                                                                            |
| ------------ | ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `packages/*` | host (`cargo nextest`) | The production crates. No live validator — but *not* network-free: e.g. `zaino-serve`'s gRPC regression test binds a loopback socket and stands up a tonic server.                                        |
| `e2e`        | ztest / k8s            | The partition driven end-to-end by a real wallet client through Zaino's gRPC surface to a live validator — a wallet's full-stack view of the indexer.                                                     |
| `clientless` | ztest / k8s            | The partition that drives a deployed Zaino over its served gRPC and JSON-RPC surfaces against a live validator, with no wallet client — fetch-vs-state backend parity, and validator-vs-Zaino oracle checks. |

## Local Ztest Cluster Setup

```sh
# 1. The CLI. It is not a workspace member; it is expected on PATH.
cargo install ztest_cli --version '^0.1' --locked

# 2. A cluster. Any reachable cluster works; kind is the usual local one.
# https://kind.sigs.k8s.io/docs/user/quick-start/#installation
kind create cluster --name ztest

# 3. Register it with ztest and provision what the harness needs.
ztest cluster add kind --kind ztest --set-default
ztest cluster setup
# This ztest cluster setup can take a few minutes, and creates k8s namespaces, monitoring, etc.

# Check if the cluster is ready/healthy
ztest cluster check
```

### Cluster storage

ztest can load validator chaindata snapshots for integration or sync tests. For Sync
tests it is much faster to use a k8s storage driver that supports CoW `VolumeSnapshot`
operations. You can skip this if just running integration tests, or accept the
performance hit of copying the 40-200GB chaindata snapshots when starting a mainnet
validator

```sh

# TopoLVM is good option when backed by lvm thin-pools
ztest cluster add kind --kind ztest --storage-driver topolvm.io --set-default

# Rook-ceph is the preferred central-cluster option
ztest cluster add kind --kind ztest --storage-driver rook-ceph.cephfs.csi.ceph.com --set-default
```
