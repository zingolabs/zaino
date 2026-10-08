# zainod

The Zaino indexer daemon. It fetches blocks from a Zebra node over JSON-RPC,
builds its own on-disk indexes, and serves wallets over the
[lightclient protocol](https://github.com/zcash/lightwallet-protocol)
(`CompactTxStreamer` gRPC, as served by
[lightwalletd](https://github.com/zcash/lightwalletd)), in plaintext HTTP/2 or
over TLS it terminates itself (`[serve.tls]`). Methods served:
[`docs/rpc_api.md`](https://github.com/zingolabs/zaino/blob/dev/docs/rpc_api.md).

The package also builds `zainodlib`, the library behind the binary (`run`,
config types). Project background: the
[Zaino repository](https://github.com/zingolabs/zaino).

## CLI

```text
zainod generate-config [--output FILE]       # write a default config file
zainod start [--config FILE]                 # start the indexer
zainod verify [--config FILE]                # read-only index check (see usage.md)
```

`--config`/`--output` default to `$XDG_CONFIG_HOME/zaino/zainod.toml`
(`$HOME/.config/zaino/zainod.toml` when unset).

## Configuration

Layered, highest priority first:

1. environment variables, prefix `ZAINO_CONFIG_`, `__` for nesting
   (`ZAINO_CONFIG_SYNC__CONCURRENCY=64`)
2. the TOML file
3. built-in defaults

Unknown keys fail the load. Sections:

- top-level `network` (`mainnet` / `testnet` / `regtest`, default `mainnet`)
- `[[trusted_validators]]`: at least one, all equal (no vote). Each is a Zebra
  JSON-RPC address with auth, timeouts and a link budget, plus an optional
  `indexer_address` (zebrad's push streams). Their headers feed the verified
  header chain, whose most-work tip is served while any of them holds it; one
  listing admits a mempool transaction; block fetches spread over all of them.
- `[serve]`: gRPC listen address, `max_address_rows`, optional `[serve.tls]`
  (`cert_path`, `key_path`)
- `[grpc]`: serving caps, `trusted_proxies`, `[grpc.shutdown]`
- `[submission]`: `propagation_threshold_secs`, `max_attempts`
- `[p2p]`: Zaino's own peers (off by default): `enabled`, `peer_target`,
  `initial_peers`, `cache_dir`
- `[sync]`: `finalised_depth`, `concurrency`, and the `queue_mib` budget every
  index shares (each index's write buffer is its own, not configured)
- one `[index.<name>]` table per index (`compact_block`, `block_hash`,
  `tree_state`, `transparent_address`, each with `enabled` and `path`)
- `[metrics]`: the optional admin listener (`listen_address`)
- `[snapshot]`: optional, `snapshot` builds only (`manifest`, `connections`)

Annotated example:
[`docs/example_configs/zainod.toml`](https://github.com/zingolabs/zaino/blob/dev/docs/example_configs/zainod.toml).

`network` is declared rather than read off the validator: Zebra on regtest
reports its chain as `"test"`. It is what `GetTreeState` reports as
`TreeState.network` and `GetLightdInfo` as `chainName`.

`index.<name>.enabled = false` builds nothing for that index (no store, no
final-stream subscription, no writer) and its methods answer `UNIMPLEMENTED`.
Any index may be disabled, compact-block included, but at least one must stay
enabled. `GetLightdInfo.blockHeight` is the served snapshot tip either way (0
before the first). Compact-block's fee index, value-balance, has no table of its
own: it runs with compact-block, in a `value_balance` directory beside
compact-block's `path`.

## Launching

Needs a Zebra node with JSON-RPC enabled (`[rpc] listen_addr` in `zebrad.toml`),
reachable at each `[[trusted_validators]] jsonrpc_address`.

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
