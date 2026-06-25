# Live Tests

The live-test suite for Zaino: tests that stand up real external processes (a
validator, and where applicable a wallet client) and exercise the assembled,
running system — kept apart from the fast unit/`container-test` development
flow. Split into two partitions, `e2e` and `integration`.

## Language

**Live test**:
A test that requires a *live* validator process (Zebra or zcashd), and
optionally a live wallet client, to exercise Zaino against real external
infrastructure. The defining property of this suite and the reason it is
excluded from the default `container-test` flow and run only by its own task
family. Umbrella over the `e2e` and `integration` partitions.
_Avoid_: integration test (means the `integration` partition here), system
test, e2e test (means the `e2e` partition here).

**e2e (test partition)**:
The partition driven end-to-end by a real wallet client (the `zcash_local_net`
devtool client) through Zaino's gRPC surface to a live validator. Tests a
wallet's full-stack view of the indexer. Formerly the `wallet-tests` package.
_Avoid_: wallet test, wallet-tests, lightclient.

**integration (test partition)**:
The partition that drives Zaino's service layer (`FetchService`/`StateService`
subscribers, RPC surface) directly against a live validator, with no wallet
client — fetch-vs-state and zcashd-vs-zainod oracle checks. Formerly the
`walletless-tests` package.
_Avoid_: walletless test, walletless-tests, clientless.

**zaino-testutils**:
The client-agnostic test harness consumed by both the lightclient and
clientless partitions. A standalone crate, not part of either partition; it
owns shared fixtures and the feature-forwarding surface (`zcashd_support`,
`transparent_address_history_experimental`).
_Avoid_: test helpers, testlib.
