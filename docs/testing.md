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

The single front door is `makers test [SET]`, where `SET` defaults to `packages`:

- `makers test` (or `makers test packages`) — the **packages** set: the
  `packages/*` production-crate tests that need no live validator.
- `makers test e2e` / `makers test clientless` — one live partition.
- `makers test live` — both live partitions (`clientless` then `e2e`) against a
  live validator, with a combined pass/fail summary.
- `makers test all` — the whole suite: packages then live.

(`container-test`, `live-clientless`, and `live-e2e` are the internal engines
the `test` front door delegates to; invoke them directly only when you need to
forward engine flags.)

### zcashd-backed tests are OFF by default

zcashd is being deprecated, so `zcashd_support` is **opt-in, not a default**
feature (docs/adr/0005): every test path runs `--no-default-features`, so the
zcashd-backed tests are compiled out. There is **no implicit or env-var path**
to enable them — only the explicit flag:

- pass the flag: `makers test --with-zcashd`, `makers test live --with-zcashd`,
  or `makers test all --with-zcashd` (it adds `--features zcashd_support`)
- use the convenience task: `makers zcashd_test` (equivalent to
  `makers test all --with-zcashd`)

See `docs/adr/0001-zcashd-support-feature-gate.md` (and `0005` for the
default-off revision) for the rationale.

### Test contention on lower-resource machines

The suites run at full parallelism (one test thread per CPU), and each
live test can spawn its own validator. On machines with fewer cores or
less RAM this can surface as occasional flaky failures caused by contention
rather than real regressions — re-running usually passes. To make runs more
reliable, lower the parallelism by reducing `test-threads` in the single root
nextest config (`.config/nextest.toml`; the live tests are additionally capped
to 6 concurrent validators via the `live-validators` test-group). For a one-off
run you can instead forward a nextest flag through the front door, e.g.
`makers test --test-threads 6`.
