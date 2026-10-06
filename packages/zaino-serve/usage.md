# `zaino-serve`

`zaino-serve` exposes the lightwalletd-compatible service and Zaino-owned
extension services on the same tonic server.

## Indexed-tip subscription

`zaino.index.v1.IndexedTipService/SubscribeIndexedTips` is a server-streaming
RPC with an empty request. It emits the canonical tip that is currently
readable through Zaino's local chain index as `{ height, hash }`.

- The first item is the current indexed tip, even if no block is mined after
  the client connects.
- Later items report canonical tip changes, including same-height reorgs.
- The stream uses Tokio watch semantics, so intermediate tips may be coalesced
  when a newer indexed state supersedes them before delivery.
- ChainHead stores a complete readable snapshot before publishing each update.
  The notification therefore describes the in-memory index/cache, not merely a
  validator announcement.
- This is not a durability notification. Recent tips live in ChainHead's
  reorg-capable in-memory window; only blocks below the finalization seam are in
  the durable finalized database.
- The gRPC routes are registered only after ChainHead has constructed an
  initial readable snapshot. Once the RPC is accepted, an initial item is
  guaranteed. Server or index shutdown ends the stream, and client cancellation
  drops its watch receiver without a producer task.

With a locally running plaintext Zaino endpoint, inspect events using:

```console
grpcurl -plaintext -max-time 30 \
  -import-path packages/zaino-proto/proto \
  -proto indexed_tip.proto \
  127.0.0.1:8137 \
  zaino.index.v1.IndexedTipService/SubscribeIndexedTips
```
