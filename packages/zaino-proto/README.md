# zaino-proto

Rust bindings (prost + tonic, server and client) for the
[lightwallet-protocol](https://github.com/zcash/lightwallet-protocol) gRPC
surface that `zaino-grpc` serves, and for zebrad's indexer service that the
chain view subscribes to. Vendored because upstream publishes `.proto` files
only, and `zcash_client_backend` generates only the client.

## Modules

- `proto::compact_formats` — `CompactBlock`, `CompactTx`, `ChainMetadata`,
  `CompactSaplingSpend`, `CompactSaplingOutput`, `CompactOrchardAction`, …
- `proto::service` — `CompactTxStreamer` messages plus
  `compact_tx_streamer_server` / `compact_tx_streamer_client`. Compact-format
  types are mapped onto `proto::compact_formats` rather than duplicated.
  `RawTransaction.data` is `bytes::Bytes`, so fanning one transaction out to
  many streams is a refcount bump.
- `proto::zebra_indexer` — zebrad's indexer service (`zebra.indexer.rpc`:
  `ChainTipChange`, `NonFinalizedStateChange`, `MempoolChange`, `GetBlock`),
  verbatim from zebra's `zebra-rpc/proto/indexer.proto`. Zaino is a client (the
  chain view's push streams, `[[trusted_validators]] indexer_address`); the
  server half serves tests' fake zebrad.
- `LIGHTWALLET_PROTOCOL_VERSION` — the vendored release (newest
  `lightwallet-protocol/CHANGELOG.md` heading), served as
  `LightdInfo.lightwalletProtocolVersion`.
- `frame` — gRPC length-prefixed framing (`[0x00][len u32 BE][message]`):
  `FRAME_HEADER`, `frame_into` (frame a message written in place), `framed_len`
  and `split_frame` (walk framed bytes). One implementation for every crate that
  writes or reads gRPC frames by hand.

## Layout

```text
lightwallet-protocol/walletrpc/   upstream v0.5.0 protos, with local edits
proto/compact_formats.proto       → symlink into lightwallet-protocol/
proto/service.proto               → symlink into lightwallet-protocol/
proto/zebra_indexer.proto         zebra's indexer.proto, verbatim
src/proto/*.rs                    generated, committed
```

Local edits to upstream `service.proto`: `Ping` / `Duration` / `PingResponse`
and `GetBlockNullifiers` removed; `GetBlockRangeNullifiers` and
`GetTaddressTxids` marked deprecated and TODO: REMOVE (served only while
pepper-sync calls them — see [`docs/rpc_api.md`](../../docs/rpc_api.md)).

## Regenerating

`build.rs` regenerates `src/proto/*.rs` only when `protoc` is available (on
`PATH` or via `PROTOC`); otherwise the committed files are used as-is. After
changing a `.proto`, build with `protoc` present and commit the regenerated
sources.

To move to a new upstream release, replace `lightwallet-protocol/` with that
tag's contents, re-apply the local edits above, regenerate, and record the
version in `CHANGELOG.md`.
