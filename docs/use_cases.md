# Indexer Live Service

`zainod` indexes the chain from a Zebra node and serves it to light clients over
lightwalletd-compatible gRPC. See [rpc_api.md](./rpc_api.md) for the methods it
serves today.

### Dependencies
1) [Zebrad](https://github.com/ZcashFoundation/zebra.git), with JSON-RPC enabled
   (`[rpc] listen_addr` in `zebrad.toml`)

### Running ZainoD
1) Build: `$ cargo build --release -p zainod`
2) Add the binary at `#PATH_TO/zaino/target/release/zainod` to PATH.

Then, in separate terminals:

3) Run `$ zebrad --config #PATH_TO_CONF/zebrad.toml start`
4) Run `$ zainod start` (uses the default config at `~/.config/zaino/zainod.toml`),
   or with an explicit config: `$ zainod start -c #PATH_TO_CONF/zainod.toml`

   To generate a default config file: `$ zainod generate-config`

Example configs for running against a Testnet Zebra node are in
[`example_configs/`](./example_configs/); see the [tutorial](./tutorial.md).

On first launch zainod syncs its index from the validator, starting at genesis;
this takes a long time on Mainnet and Testnet. The index is persisted in the
store directory (`[store] path`), so a restarted zainod resumes from where it
stopped.
