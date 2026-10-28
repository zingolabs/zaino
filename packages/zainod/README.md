# zainod

The Zaino indexer daemon. It follows the Zcash chain from one or more trusted Zebra validators
(JSON-RPC, or blocks over their indexer gRPC) and, optionally, Zcash p2p peers, builds its own
on-disk indexes, and serves wallets over the
[lightclient protocol](https://github.com/zcash/lightwallet-protocol) (`CompactTxStreamer` gRPC,
as served by [lightwalletd](https://github.com/zcash/lightwalletd)).

```sh
cargo install --locked --git https://github.com/zingolabs/zaino zainod
zainod generate-config     # writes $XDG_CONFIG_HOME/zaino/zainod.toml; edit it
zainod start
```

`zainod --help` lists every subcommand: `start`, `generate-config`, `verify`, `hold`, `resume`
and `overload`.

- [Running zainod](https://github.com/zingolabs/zaino/blob/dev/docs/running.md): configuration,
  states, operator hold, systemd, index snapshots, `verify`, network exposure.
- [Annotated config](https://github.com/zingolabs/zaino/blob/dev/docs/example_configs/zainod.toml):
  every key and its default.
- [zainod usage](https://github.com/zingolabs/zaino/blob/dev/packages/zainod/usage.md): failure
  policy, logging.
- [RPC API](https://github.com/zingolabs/zaino/blob/dev/docs/rpc_api.md): the methods served.
- [Container image](https://github.com/zingolabs/zaino/blob/dev/docs/docker.md).

## License

Apache-2.0.
