# Testing
### Dependencies
1) [Zebrad](https://github.com/ZcashFoundation/zebra.git)
2) [Lightwalletd](https://github.com/zcash/lightwalletd.git)
3) [Zcashd, Zcash-Cli](https://github.com/zcash/zcash)

### Tests
1) Simlink or copy compiled `zebrad`, zcashd` and `zcash-cli` binaries to `$ zaino/test_binaries/bins/*`

Chain Cache: Several tests rely on a cached chain to run, for these tests to pass the chain must first be generated:
2) Generate the zcashd chain cache `cargo nextest run generate_zcashd_chain_cache --run-ignored ignored-only`
3) Generate the zebrad chain cache `cargo nextest run generate_zebrad_large_chain_cache --run-ignored ignored-only`

Client Rpc Tests: For the client rpc tests to pass a Zaino release binary must be built and added to PATH:
4) Build release binary `cargo build --release` and add to PATH. WARNING: these tests do not use the binary built by cargo nextest

5) Run `$ cargo nextest run`

NOTE: The client rpc get_subtree_roots tests are currently ignored, to run them testnet and mainnet chains must first be generated.
- To run client rpc test `get_subtree_roots_sapling`:
1) sync Zebrad testnet to at least 2 sapling shards
2) copy the Zebrad testnet `state` cache to `zaino/integration-tests/chain_cache/get_subtree_roots_sapling` directory.
See the `get_subtree_roots_sapling` test fixture doc comments in infrastructure for more details.

- To run client rpc test `get_subtree_roots_orchard`:
1) sync Zebrad mainnet to at least 2 orchard shards
2) copy the Zebrad mainnet `state` cache to `zaino/integration-tests/chain_cache/get_subtree_roots_orchard` directory.
See the `get_subtree_roots_orchard` test fixture doc comments in infrastructure for more details.

- TESTNET TESTS:
the testnet tests are temporary and will be replaced with regtest as soon as (https://github.com/zingolabs/zaino/issues/231) is resolved
In the mean time, these tests can be ran, but it is a fiddly process. First, it needs a zebrad fully synced to testnet (depending
on internet speed, etc., this could take 10+ hours). Then, the tests must be run 1 at a time (passing `--no-capture` will enforce this).
Furthermore, due to https://github.com/zingolabs/infrastructure/issues/43, sometimes a zebrad will persist past the end of the test and
hold a lock on the testnet cache, causing all remaining tests to fail. This process must be stopped manually, in order to allow testnet
tests to work again.
