# Testing
### Dependencies
1) [Zebrad](https://github.com/ZcashFoundation/zebra.git)
2) [Lightwalletd](https://github.com/zcash/lightwalletd.git)
3) [Zcashd, Zcash-Cli](https://github.com/zcash/zcash)

### Tests
1) Symlink or copy compiled `zebrad`, `zcashd` and `zcash-cli` binaries to `zaino/live-tests/test_binaries/bins/*`
2) Add `zaino/live-tests/test_binaries/bins` to `$PATH` or to `$TEST_BINARIES_DIR`
3) Run `cargo nextest run`

The expected versions of these binaries is detailed in the file ``.env.testing-artifacts`.

## Cargo Make
Another method to work with tests is using `cargo make`, a Rust task runner and build tool.
This can be installed by running `cargo install --force cargo-make` which will install cargo-make in your ~/.cargo/bin.
From that point you will have two executables available: `cargo-make` (invoked with `cargo make`) and `makers` which is invoked directly and not as a cargo plugin.

`cargo make help`
will print a help output.
`Makefile.toml` holds a configuration file.

## Containerized test tasks (podman)

The `makers` tasks below build and run the test suites inside a **podman**
container, so you don't need the validator binaries on your host `$PATH`. The
container image is built or pulled automatically on first run.

- `makers offline-tests` — runs the **offline** suite: the `packages/*`
  production-crate tests that need no live validator.
- `makers live-tests` — runs the **live** suite: both partitions (`integration`
  then `e2e`) against a live validator, and prints a combined pass/fail summary.
- `makers all-tests` — runs the whole suite: offline then live.

(`container-test`, `live-integration`, and `live-e2e` are the internal engines
the front doors delegate to; invoke them directly only when you need a single
partition or to forward engine flags.)

### zcashd-backed tests are OFF by default

zcashd is being deprecated, so the suites compile with `--no-default-features`
(the default-on `zcashd_support` feature turned OFF), and the zcashd-backed
tests are compiled out. To include them, turn the feature back on in any of
these equivalent ways:

- pass the flag: `makers offline-tests --with-zcashd`,
  `makers live-tests --with-zcashd`, or `makers all-tests --with-zcashd`
- set the env var: `CONTAINER_TEST_WITH_ZCASHD=1 makers all-tests`
- use the convenience task: `makers zcashd_test` (equivalent to
  `makers all-tests --with-zcashd`)

See `docs/adr/0001-zcashd-support-feature-gate.md` for the rationale.

### Test contention on lower-resource machines

The suites run at full parallelism (one test thread per CPU), and each
integration test can spawn its own validator. On machines with fewer cores or
less RAM this can surface as occasional flaky failures caused by contention
rather than real regressions — re-running usually passes. To make runs more
reliable, lower the parallelism by reducing `test-threads` in the single root
nextest config (`.config/nextest.toml`; the live tests are additionally capped
to 6 concurrent validators via the `live-validators` test-group). For a one-off
run you can instead forward a nextest flag through any front door, e.g.
`makers offline-tests --test-threads 6`.
