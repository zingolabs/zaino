# Live Tests

The live-test suite for Zaino: tests that stand up real external processes (a
validator, and where applicable a wallet client) and exercise the assembled,
running system — kept apart from the fast unit/`container-test` development
flow. Split into two partitions, `e2e` and `clientless`.

## Language

**Live test**:
A test that requires a *live* validator process (Zebra or zcashd), and
optionally a live wallet client, to exercise Zaino against real external
infrastructure. The defining property of this suite and the reason it is
excluded from the default `container-test` flow and run only by its own task
family. Umbrella over the `e2e` and `clientless` partitions.
_Avoid_: integration test (now reserved for Cargo's sense — any `tests/*.rs`;
the no-client live partition is `clientless`, see docs/adr/0004), system test,
e2e test (means the `e2e` partition here).

**e2e (test partition)**:
The partition driven end-to-end by a real wallet client (the `zcash_local_net`
devtool client) through Zaino's gRPC surface to a live validator. Tests a
wallet's full-stack view of the indexer. Formerly the `wallet-tests` package.
_Avoid_: wallet test, wallet-tests, lightclient.

**clientless (test partition)**:
The partition that drives Zaino's service layer (`FetchService`/`StateService`
subscribers, RPC surface) directly against a live validator, with no wallet
client — fetch-vs-state and zcashd-vs-zainod oracle checks. Formerly the
`walletless-tests` package; named `integration` until docs/adr/0004 reverted
that to `clientless`. The crate/dir is `live-tests/clientless`; selected with
`-p clientless` or `makers test clientless`.
_Avoid_: walletless test, walletless-tests; integration (the former name —
reverted in docs/adr/0004, now reserved for Cargo's sense).

**zaino-testutils**:
The client-agnostic test harness consumed by both the `e2e` and `clientless`
partitions. A standalone crate, not part of either partition; it
owns shared fixtures and the feature-forwarding surface (`zcashd_support`,
`transparent_address_history_experimental`).
_Avoid_: test helpers, testlib.

**packages (test set)**:
The tests of the `packages/*` production crates — the workspace
`default-members`, exercised by bare `cargo nextest run` and by `makers test`
(the default with no argument, equivalently `makers test packages`). Named for
where the crates live (`packages/`); the functional reason they run apart from
the live partitions is that they need **no** live validator. They are *not*
network-free, though: e.g. zaino-serve's gRPC `spawn` regression test binds a
loopback socket and stands up a tonic server. The absent ingredient is a
validator, not the network.
_Avoid_: package (singular — the set is plural, matching the `packages/` dir;
`makers test package` now errors with `unknown set`); offline (overclaims "no
network" — these tests bind loopback sockets); unit test (the set includes
crate-level integration tests); container test (names the run mechanism, which
the live suite shares).
