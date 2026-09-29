# zainod

The Zaino indexer daemon. It fetches blocks from a Zebra node over JSON-RPC,
builds its own on-disk indexes, and serves wallets over the
[lightclient protocol](https://github.com/zcash/lightwallet-protocol)
(`CompactTxStreamer` gRPC, as served by
[lightwalletd](https://github.com/zcash/lightwalletd)), in plaintext. Methods
served: [`docs/rpc_api.md`](https://github.com/zingolabs/zaino/blob/dev/docs/rpc_api.md).

The package also builds `zainodlib`, the library behind the binary (`run`,
config types). Project background: the
[Zaino repository](https://github.com/zingolabs/zaino).

## CLI

```text
zainod generate-config [--output FILE]       # write a default config file
zainod start [--config FILE]                 # start the indexer
zainod verify [--config FILE] [--rehash]     # read-only index check (see usage.md)
```

`--config`/`--output` default to `$XDG_CONFIG_HOME/zaino/zainod.toml`
(`$HOME/.config/zaino/zainod.toml` when unset).

## Configuration

Layered, highest priority first:

1. environment variables, prefix `ZAINO_CONFIG_`, `__` for nesting
   (`ZAINO_CONFIG_SOURCE__JSONRPC_ADDRESS=127.0.0.1:8232`)
2. the TOML file
3. built-in defaults

Unknown keys fail the load. The config has `[source]` (Zebra JSON-RPC address
and auth), `[[chainview_peers]]` (extra validators: the quorum tip and mempool
are agreed over `source` + these, and bulk sync spreads its fetches over all of
them), `[serve]` (gRPC listen address, `max_block_range`), `[grpc]` (serving
caps), `[fetch]` (`finalised_depth`, `concurrency`, and `primary_validator` to
pin bulk sync to one validator), one `[index.<name>]` section per index
(`compact_block`, `tree_state`, `transparent_address`, each with `path`,
`batch`, `queue_mib` and `enabled`), a top-level `network` (`mainnet` /
`testnet` / `regtest`, default `mainnet`) and an optional top-level
`metrics_endpoint` (Prometheus, with the `prometheus` feature). Annotated
example:
[`docs/example_configs/zainod.toml`](https://github.com/zingolabs/zaino/blob/dev/docs/example_configs/zainod.toml).

`network` is declared rather than read off the validator: Zebra on regtest
reports its chain as `"test"`. It is what `GetTreeState` reports as
`TreeState.network` and `GetLightdInfo` as `chainName`.

`index.<name>.enabled = false` builds nothing for that index (no store, no
`BlockSink` subscription, no follower) and its methods answer `UNIMPLEMENTED`.
`index.compact_block` cannot be disabled: `GetLightdInfo` reads its finalised
height.

## Launching

Needs a Zebra node with JSON-RPC enabled (`[rpc] listen_addr` in `zebrad.toml`),
reachable at `source.jsonrpc_address`.

```sh
cargo install zainod                         # or: cargo run --release -p zainod -- start ...
zainod generate-config                       # then edit it
zainod start
```

For the container image (non-root, UID 1000; rootless Podman works with
`--userns=keep-id`) see
[`docs/docker.md`](https://github.com/zingolabs/zaino/blob/dev/docs/docker.md).

## License

Apache-2.0.
