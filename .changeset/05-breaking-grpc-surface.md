---
default: major
---

# `zainod`: gRPC methods removed, and answers that changed

Removed (each answers `UNIMPLEMENTED`):

- `zaino.index.v1.IndexedTipService` (added in 0.10.1): poll `GetLatestBlock` instead.
- `GetBlockNullifiers`: use `GetBlock`, or `GetBlockRange` with `poolTypes`.
- `Ping`.

Still served, as deprecated aliases that will be removed: `GetBlockRangeNullifiers`
(= `GetBlockRange`, nullifiers only) and `GetTaddressTxids` (= `GetTaddressTransactions`).

Answers that changed:

- `GetLightdInfo` reads no validator: `blockHeight` = the served tip, `estimatedHeight` = the
  best verified header, consensus branch and next upgrade from the upgrade schedule at the served
  tip. `lightwalletProtocolVersion` = `v0.6.0`. `donationAddress` is empty. Before anything is
  served it answers `UNAVAILABLE`.
- A method whose index is disabled answers `UNIMPLEMENTED`. While zainod starts up, every method
  answers `UNAVAILABLE`.
- Requests by height (`GetBlock`, `GetBlockRange`, `GetTreeState`) answer up to their own index's
  durable tip, which during a sync can be above the served tip. `GetLatestBlock`,
  `GetLatestTreeState`, `GetSubtreeRoots` and the address methods answer at the served tip.
- `GetBlockRange` with `start > end` streams in decreasing height order. A range reaching past
  the tip streams every block up to it, then ends `OUT_OF_RANGE`, as lightwalletd does.
- `poolTypes`: an empty list = Sapling, Orchard and Ironwood (transparent excluded). An unknown
  value is `INVALID_ARGUMENT`. `GetMempoolTx` filters by `poolTypes` as `GetBlockRange` does.
- `GetTreeState` below Sapling activation answers that height with every tree `""`.
- `GetSubtreeRoots` with an unknown `shieldedProtocol` is `UNIMPLEMENTED`.
- `GetTransaction` asks the trusted validators for the bytes. It is `NOT_FOUND` only when every
  validator asked says absent. A transaction not on the best chain reports height `0`.
- One address request walks at most `[serve] max_address_rows` (100,000) receives across its
  addresses, and is `RESOURCE_EXHAUSTED` past that, never a partial answer.
- The client's `grpc-timeout` is its deadline (`DEADLINE_EXCEEDED`). Streams have no total-duration
  cap.
- Request bodies are capped at 64 KiB, or 2 MB + 1 KiB for `SendTransaction`
  (`RESOURCE_EXHAUSTED`).
- `GetMempoolTx` and `GetMempoolStream` list a transaction once any trusted validator lists it
  (or the `[p2p]` peer quorum admits it).
- `GetMempoolStream` ends when the served tip moves, so the `GetLatestBlock` that follows serves
  the new block. It never replays transactions the served tip already mined.
