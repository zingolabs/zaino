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

## Which zebrad the live suite runs

Live tests name their validator with `zebra!()`, never a version literal. It is
declared once, in `live-tests/Cargo.toml`:

```toml
[workspace.metadata.zaino.zebra]
source = "published"
version = "6.2.3"
dockerfile = "docker/Dockerfile"
```

`version` is both the `zfnd/zebra` image tag and the semver ztest gates the
generated regtest config on — it decides which NU6.x activation-height keys are
emitted — so it is declared for a fork build too.

### Testing against a zebra fork

zainod links zebra as libraries while the validator is a separate container
image, so a fork has to reach both. Patch the libraries at the repo root, which
`cargo install --locked` in the `Dockerfile` requires anyway, and the validator
follows from the same commit:

```toml
# Cargo.toml (repo root)
[patch.crates-io]
zebra-chain = { git = "https://github.com/me/zebra", branch = "my-wip" }
zebra-state = { git = "https://github.com/me/zebra", branch = "my-wip" }
zebra-rpc = { git = "https://github.com/me/zebra", branch = "my-wip" }
```

```toml
# live-tests/Cargo.toml
[workspace.metadata.zaino.zebra]
source = "patch"
```

```sh
cargo update -p zebra-chain   # resolves the branch, records the commit
cd live-tests && ztest run
```

Cargo records the resolved commit in the root `Cargo.lock` whether the patch
asked for a `branch`, a `tag` or a `rev`, and the validator is built on-cluster
from that commit. So the fork is named once and the validator cannot drift from
the indexer. Iterate by pushing to the branch and re-running `cargo update`.

Declaring a `branch` is preferred, and only the resolved commit ever reaches
ztest — which matters, because ztest keys its git-fetch cache and its image tag
on the ref it is given and treats both as immutable. A `path =` patch is
refused: the cluster cannot reach your worktree.

### Overrides

Highest precedence first:

| Variable                                | Effect                                                     |
| --------------------------------------- | ---------------------------------------------------------- |
| `ZAINO_ZEBRA_GIT` + `ZAINO_ZEBRA_REV`   | Build from this url and commit. Both or neither.           |
| `ZAINO_ZEBRA_SOURCE`                    | `published` or `patch`, overriding the manifest.           |
| `ZAINO_ZEBRA_VERSION`                   | Retag the release, or set a fork build's semver.           |
| `ZAINO_ZEBRA_DOCKERFILE`                | Dockerfile path within the zebra tree.                     |

To keep a local setting across runs without it showing up in `git status`, put
it in `live-tests/.cargo/config.toml` — `.cargo` is gitignored:

```toml
[env]
ZAINO_ZEBRA_VERSION = "6.3.0"
```
