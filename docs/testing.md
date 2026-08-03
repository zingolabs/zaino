# Testing

Zaino has two kinds of tests, run by two different mechanisms:

- **Production-crate tests** — the unit and crate-level integration tests of the
  `packages/*` crates (the root workspace `default-members`). They need no live
  validator and run locally with a plain `cargo nextest run`.
- **Live tests** — tests that stand up a real validator (Zebra or zcashd), and
  where applicable a wallet client, and exercise the assembled, running system.
  They live in the standalone `live-tests/` workspace and run on the **ztest**
  Kubernetes harness, not on your host.

The single front door for both is `makers test [SET]` (cargo-make). It routes
the production set to `cargo nextest` and the live sets to `ztest run`.

## Quick start

```sh
makers test                # packages set (default): packages/* tests, no validator
makers test packages       # same as above, explicit
makers test e2e            # the e2e live partition        (ztest / k8s)
makers test clientless     # the clientless live partition (ztest / k8s)
makers test live           # both live partitions          (ztest / k8s)
makers test all            # everything: packages, then live
makers test ironwood       # the ironwood-pool tests across every set
```

Anything after the set name is forwarded verbatim to the underlying runner, so
every `cargo nextest run` flag works — e.g. `makers test --no-fail-fast`,
`makers test packages -E 'test(regtest)'`, `makers test --test-threads 6`.

`cargo-make` is installed with `cargo install --force cargo-make`; it provides
both `cargo make` and the standalone `makers`. `makers help` lists every task.

## The test sets

| Set          | Where it runs          | What it covers                                                                                                                                                                                                        |
| ------------ | ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `packages`   | host (`cargo nextest`) | The `packages/*` production crates. No live validator — but *not* network-free: e.g. `zaino-serve`'s gRPC regression test binds a loopback socket and stands up a tonic server.                                       |
| `e2e`        | ztest / k8s            | The partition driven end-to-end by a real wallet client through Zaino's gRPC surface to a live validator — a wallet's full-stack view of the indexer.                                                                 |
| `clientless` | ztest / k8s            | The partition that drives Zaino's service layer (`FetchService`/`StateService` subscribers, RPC surface) directly against a live validator, with no wallet client — fetch-vs-state and zcashd-vs-zebra oracle checks. |
| `live`       | ztest / k8s            | `clientless` + `e2e` together.                                                                                                                                                                                        |
| `all`        | both                   | `packages`, then `live`. Runs every set even if an earlier one fails; the exit code reflects any failure.                                                                                                             |
| `ironwood`   | both                   | The Ironwood-pool selection across every set: the two dedicated live binaries (`ironwood_activation`, `the_pub_testnet_ironwood_boundary`) plus every test whose name carries `ironwood`.                             |

The two live partitions live under `live-tests/` as their own crates
(`live-tests/e2e`, `live-tests/clientless`), sharing the client-agnostic harness
crate `live-tests/zaino-testutils`. That whole tree is a **standalone Cargo
workspace** — separate from the production workspace and its lockfile — because
it links no production code and pulls in the heavy, fast-moving ztest/validator
dependency graph.

## Production tests (the `packages` set)

`makers test packages` is a thin wrapper over `cargo nextest run` on the root
workspace `default-members`. You can equivalently run `cargo nextest run`
directly. Requirements on the host: a C/C++ toolchain, `protoc`, and RocksDB
headers (the nix dev shell provides all three). To skip the slow bundled RocksDB
build and link the system copy, see the `use-system-rocksdb` task
(`makers use-system-rocksdb`).

The same production set is what CI runs: it archives the workspace with
`cargo nextest archive` and runs each production crate as a matrix partition (see
`.github/workflows/ci.yml`).

## Live tests (ztest / Kubernetes)

The live partitions run on the **ztest** harness, which schedules each test's
validator (and wallet client) as pods on a Kubernetes cluster. ztest is not
vendored into this repo; it lives in the sibling checkout that
`live-tests/Cargo.toml` pins (`../../ztest`, relative to `live-tests/`). The
front door shells out to its `ztest run` CLI with `live-tests/` as the working
directory, forwarding your extra args to the wrapped `cargo nextest run`.

Running the live sets therefore requires:

- the `../ztest` sibling checkout alongside this repo,
- a reachable cluster (a `KUBECONFIG` / kube context ztest can `infer()`), and
- image build/registry access for the per-test pod images.

See the ztest documentation for cluster setup, the QoS tiers, and the dev-image
build pipeline.

## zcashd-backed tests are opt-in

zcashd is being deprecated, so `zcashd_support` is an **opt-in feature, not a
default** (see `docs/adr/0001-zcashd-support-feature-gate.md` and
`docs/adr/0005-zcashd-support-default-off.md`). Every default test path builds
with the feature off, so the zcashd-backed tests compile out. There is no
implicit or env-var switch — enable them by forwarding the Cargo feature through
the front door:

```sh
makers test packages --features zcashd_support
makers test live --features zcashd_support
```

## Test contention on lower-resource machines

The production set runs at full parallelism (one test thread per CPU). On
machines with fewer cores or less RAM this can surface as occasional flaky
failures caused by contention rather than real regressions — re-running usually
passes. Lower the parallelism for a run by forwarding a nextest flag through the
front door, e.g. `makers test --test-threads 6`.

## The CI container image

CI runs its jobs inside the `zingodevops/zaino-ci` image — a Rust build
environment (toolchain, `protoc`, RocksDB, `cargo-nextest`) built from
`live-tests/test_environment/Containerfile`. It carries no validator binaries;
the live suites get their validators from ztest pods, not from this image. The
image is built/tagged/pushed by the `build-image` / `push-image` cargo-make
tasks and the `build-n-push-ci-image` workflow; its tag is derived from the
pinned Rust toolchain (`rust-toolchain.toml`) and a content hash of the image
build context (`live-tests/test_environment/`), so any change to either yields a
new tag. The single source for that computation is
`tools/scripts/get-ci-image-tag.sh`.
