# Testing
### Dependencies
1) [Zebrad](https://github.com/ZcashFoundation/zebra.git)
2) [Lightwalletd](https://github.com/zcash/lightwalletd.git)
3) [Zcashd, Zcash-Cli](https://github.com/zcash/zcash)

### Tests
1) Symlink or copy compiled `zebrad`, `zcashd` and `zcash-cli` binaries to `zaino/test_binaries/bins/*`

**Client Rpc Tests** _For the client rpc tests to pass a Zaino release binary must be built and added to PATH.
WARNING: these tests do not use the binary built by cargo nextest._

Note: Recently the newest GCC version on Arch has broken a build script in the `rocksdb` dependency. A workaround is:
`export CXXFLAGS="$CXXFLAGS -include cstdint"`

4) Build release binary `cargo build --release` and add to PATH. For example, `export PATH=./target/release:$PATH`

5) Run `cargo nextest run`

NOTE: The client rpc get_subtree_roots tests are currently ignored, to run them testnet and mainnet chains must first be generated.

To run client rpc test `get_subtree_roots_sapling`:
1) sync Zebrad testnet to at least 2 sapling shards
2) copy the Zebrad testnet `state` cache to `zaino/integration-tests/chain_cache/get_subtree_roots_sapling` directory.

See the `get_subtree_roots_sapling` test fixture doc comments in infrastructure for more details.

To run client rpc test `get_subtree_roots_orchard`:
1) sync Zebrad mainnet to at least 2 orchard shards
2) copy the Zebrad mainnet `state` cache to `zaino/integration-tests/chain_cache/get_subtree_roots_orchard` directory.

See the `get_subtree_roots_orchard` test fixture doc comments in infrastructure for more details.

- TESTNET TESTS:
The testnet tests are temporary and will be replaced with regtest as soon as (https://github.com/zingolabs/zaino/issues/231) is resolved.
In the mean time, these tests can be ran, but it is a fiddly process. First, it needs a zebrad fully synced to testnet (depending on internet speed, etc., this could take 10+ hours).
To build the zebra testnet cache, the best way is to use zebra directly. With `zebrad` already in `$PATH` and from the `zaino/` directory run `zebrad --config ./zainod/zebrad.toml start`.
Then, the tests must be run 1 at a time (passing `--no-capture` will enforce this).
Furthermore, due to https://github.com/zingolabs/infrastructure/issues/43, sometimes a zebrad may persist past the end of the test and hold a lock on the testnet cache, causing remaining tests to fail. This process can be stopped manually, in order to allow testnet tests to work again.
