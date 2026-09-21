# Tutorial: run zainod against a local Zebra node

Step 0: Check out zaino.

Step 1: Install zebra v3.1.0.

```
git clone git@github.com:ZcashFoundation/zebra.git
cd zebra
git checkout v3.1.0
cargo install --path zebrad --locked
```

Step 2: Edit the example configs. From the zaino repo root, replace
`<PATH_TO_ZEBRA>` in `docs/example_configs/zebrad_config_3.1.0.toml` and
`<ZAINO_STORE>` in `docs/example_configs/zainod.toml` with writable directories.
The pair is set up for Testnet: zebrad serves JSON-RPC on `127.0.0.1:18231`,
which is zainod's `source.jsonrpc_address`.

Step 3: Start zebrad:

```
zebrad -c docs/example_configs/zebrad_config_3.1.0.toml start
```

Step 4: In another shell, start zainod:

```
cargo run --release -p zainod -- start -c docs/example_configs/zainod.toml
```

zainod serves lightwalletd-compatible gRPC on `127.0.0.1:8137`.
