# zaino-proto

Rust bindings (prost + tonic) for the
[lightwallet-protocol](https://github.com/zcash/lightwallet-protocol) gRPC
surface that `zaino-grpc` serves, and for zebrad's indexer service that the
chain view subscribes to. Vendored for the local `service.proto` edits below and
the `bytes::Bytes` fields.

## Modules

- `proto::compact_formats` — `CompactBlock`, `CompactTx`, `ChainMetadata`,
  `CompactSaplingSpend`, `CompactSaplingOutput`, `CompactOrchardAction`, …
- `proto::service` — `CompactTxStreamer` messages plus
  `compact_tx_streamer_client` (no server stub). Compact-format
  types are mapped onto `proto::compact_formats` rather than duplicated.
  `RawTransaction.data` is `bytes::Bytes`, so fanning one transaction out to
  many streams is a refcount bump.
- `proto::zebra_indexer` — zebrad's indexer service (`zebra.indexer.rpc`:
  `ChainTipChange`, `NonFinalizedStateChange`, `MempoolChange`, `GetBlock`,
  `GetBlockRange`): zebra's `zebra-rpc/proto/indexer.proto` plus `GetBlockRange`
  (Zaino's zebra fork). Zaino is a client (`zaino-source`: push streams and block
  fetch, `[[trusted_validators]] indexer_address`); the server half serves tests'
  fake zebrad.
- `LIGHTWALLET_PROTOCOL_VERSION` — the vendored release (newest
  `lightwallet-protocol/CHANGELOG.md` heading), served as
  `LightdInfo.lightwalletProtocolVersion`.
- `frame` — gRPC length-prefixed framing (`[0x00][len u32 BE][message]`):
  `FRAME_HEADER`, `frame_into` (frame a message written in place), `framed_len`
  and `split_frame` (walk framed bytes). One implementation for every crate that
  writes or reads gRPC frames by hand.

## Layout

```text
lightwallet-protocol/walletrpc/   upstream protos (release: CHANGELOG.md), with local edits
proto/compact_formats.proto       → symlink into lightwallet-protocol/
proto/service.proto               → symlink into lightwallet-protocol/
proto/zebra_indexer.proto         zebra's indexer.proto + GetBlockRange
```

Local edits to upstream `service.proto`: `Ping` / `Duration` / `PingResponse`
and `GetBlockNullifiers` removed; `GetBlockRangeNullifiers` and
`GetTaddressTxids` marked deprecated and TODO: REMOVE (served only while
pepper-sync calls them — see [`docs/rpc_api.md`](../../docs/rpc_api.md)).

## Generation

`build.rs` generates the bindings into `OUT_DIR` on every build, so building
needs `protoc` (on `PATH`, or named by `PROTOC`). Nothing generated is
committed. `CompactTxStreamer` is generated client-only: `zaino-grpc` serves it
by dispatching on the request path, not through a tonic server.

To move to a new upstream release, replace `lightwallet-protocol/` with that
tag's contents, re-apply the local edits above, and record the change in a
`.changeset/` file ([RELEASING.md](../../RELEASING.md)).
