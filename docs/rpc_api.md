# Zaino's RPC surface

`zainod` serves the lightwalletd-compatible `CompactTxStreamer` gRPC service
defined by the
[LightWallet Protocol](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/service.proto)
(message types in
[compact_formats.proto](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/compact_formats.proto)),
on `serve.grpc_listen_address` (default `127.0.0.1:8137`) over HTTP/2, plaintext
unless `[serve.tls]` is set.
Zaino serves no JSON-RPC. Node queries go to the validator's own JSON-RPC.

## Where each method is answered

Which side a method falls on is not a per-method judgement. It follows from
[architecture §1](./architecture.md#1-who-owns-what): derived answers come from a
Zaino index (never the validator), and writes and point lookups of primary
consensus objects are forwarded to the validator.

| Method                                                  | Answered by                                                                                                                                    |
| ------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `GetLatestBlock`                                        | the served tip (no index read)                                                                                                                 |
| `GetBlock`, `GetBlockRange`                             | compact-block index                                                                                                                            |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` | tree-state index                                                                                                                               |
| `GetAddressUtxos`, `GetAddressUtxosStream`              | transparent-address index                                                                                                                      |
| `GetTaddressBalance`, `GetTaddressBalanceStream`        | transparent-address index                                                                                                                      |
| `GetTaddressTransactions`                               | transparent-address index for the `{height, txid}` set, the validator for the bytes                                                            |
| `GetTransaction`                                        | validator                                                                                                                                      |
| `GetStandardFee`                                        | computed (no validator call): the ZIP 317 marginal fee for the block after best, zatoshis per logical action (`zaino_primitives::protocol::standard_fee`: zebrad's rule), estimator version 0 |
| `SendTransaction`                                       | chain view [submission](./architecture.md#6-submission): a trusted `checkadmission` answers, then relayed through the peers                    |
| `GetMempoolTx`, `GetMempoolStream`                      | chain view [mempool](./architecture.md#5-the-mempool-view): listed by a trusted validator, admitted by a peer quorum, or our own accepted send |
| `GetLightdInfo`                                         | the served `GrpcSnapshot`: its tip, the verified best, the upgrade schedule (no validator call)                                                |
| `GetBlockRangeNullifiers` *(deprecated, TODO: REMOVE)*  | compact-block index, re-projected to nullifiers                                                                                                |
| `GetTaddressTxids` *(deprecated, TODO: REMOVE)*         | as `GetTaddressTransactions`                                                                                                                   |

"The validator" here is the traffic balancer (`zaino-traffic`) over every
`[[trusted_validators]]` entry: lowest `priority` tier first, the least loaded
within it, a slow answer hedged and an absent one asked of the next until one
answers (a lagging validator may lack a just-mined transaction); `NOT_FOUND` only
when every one asked said absent. The two deprecated rows are served
only because pepper-sync calls them (see
[client-requirements.md](./client-requirements.md)).

A `BlockID` that carries a hash, in `GetBlock` or `GetTreeState`, is resolved to
a height through the block-hash index. A hash wins over a height when both are
given, because a hash names one block across a reorg and a height does not.

Every index can be disabled (at least one stays on); compact-block's fee index,
utxo-set (every `CompactTx.fee`), runs with it and has no switch of its
own. A disabled index's methods answer `UNIMPLEMENTED` (for the block-hash
index, only the by-hash form of `GetBlock` and `GetTreeState`). Nothing falls
back to the validator.

Every index method reads one `GrpcSnapshot` of every index, pinned for the request or
stream. A method naming no height answers at the served tip, which every
enabled index holds: `GetLatestBlock`, `GetLatestTreeState`, `GetSubtreeRoots`,
`GetLightdInfo.blockHeight`, and the address methods (a `GetTaddressTxids` range
included). While Zaino syncs, the served tip is the lowest tip across the
enabled indexes, so a wallet gating on `GetLatestBlock` or
`GetLightdInfo.blockHeight` never asks past what every index holds. A block or
tree-state method naming a height (`GetBlock`, `GetBlockRange`, `GetTreeState`)
answers up to its own index's tip, which during sync may be above the served
tip. Before the first `GrpcSnapshot` (indexes opening at boot) every index
method is `UNAVAILABLE`.

`GetLightdInfo` never calls a validator, so it answers while no trusted validator vouches too.
`blockHeight` is the served tip, `estimatedHeight` the verified best, `chainName` the configured
network, and the branch and next upgrade come from the upgrade schedule at the served tip
(compiled in for mainnet and testnet, the validator's for regtest). `version` is zainod's.
`lightwalletProtocolVersion` is the vendored protos' release (the newest heading of
`packages/zaino-proto/lightwallet-protocol/CHANGELOG.md`); pepper-sync refuses one below `v0.5.0`
([client-requirements.md](./client-requirements.md)). Before anything is served the call is
`UNAVAILABLE`.

`GetTransaction` answers a mined transaction with its height, and an unmined or
orphaned one with height 0, the wire's only "no height" value.

`SendTransaction` reports a rejection in the `SendResponse` itself, as
lightwalletd does: gRPC `OK`, `errorCode` = the validator's own JSON-RPC code
and `errorMessage` = its message verbatim (`-22` for bytes that do not parse,
`-25` for the expiry and consensus-branch prechecks Zaino runs itself, as zebrad
would answer them). A wallet has to tell "the network said no" from "the network
is unreachable", and zingolib matches the message text ("transaction already
exists in mempool", "transaction already in block chain", "already queued for
download"). zebrad's `-1` answers that judge the transaction itself count as
rejections; its other `-1`s (queue full, mempool disabled) do not. The call only
fails with a status when no validator accepted and at least one could not be
reached. An accepted transaction answers `errorCode` 0 and the quoted
display-order txid.

`GetBlockRange` reaching past the compact-block index's tip streams every block
up to it, then ends with `OUT_OF_RANGE` in its trailers, never an early `OK`
(the iOS downloader waits forever after one). A range whose first height is
past it is `OUT_OF_RANGE` before any block.

`GetTreeState` answers exactly the height asked for, with the block hash in
display order. Each tree is `""` below its pool's activation and the serialized
tree from activation on, `000000` while empty (pepper-sync rejects `""` for an
active pool). Below Sapling activation every tree is `""`.

`GetSubtreeRoots` answers each request whole or refuses it before the first
byte. A `startIndex` past the last root is an empty `OK` stream, which
pepper-sync reads as the end, and `maxEntries` 0 means every root. A
`shieldedProtocol` Zaino does not know is `UNIMPLEMENTED`, the one refusal the
Android SDK survives without dropping its fast sync.

No stream has a total-duration cap. The client's `grpc-timeout` is its
deadline, and `[grpc] unread_timeout_secs` / `silent_timeout_secs` bound a stream that makes
no progress (see [running.md](./running.md#stream-time-limits)).

## Status codes a client must distinguish

| Condition                                                                                           | Code                 | Read it as                                                             |
| --------------------------------------------------------------------------------------------------- | -------------------- | ---------------------------------------------------------------------- |
| nothing served yet (indexes opening at boot)                                                        | `UNAVAILABLE`        | back off and retry                                                     |
| work-stream or subscription cap full (`grpc-retry-pushback-ms: 250`)                                | `UNAVAILABLE`        | retry after the hint                                                   |
| no verified tip, or neither a trusted validator holding it nor a peer quorum (mempool methods)      | `UNAVAILABLE`        | back off and retry                                                     |
| validator unreachable (`GetTransaction`, `SendTransaction`)                                         | `UNAVAILABLE`        | back off and retry                                                     |
| stream with nothing to send for `silent_timeout_secs` (not `GetMempoolStream`)                      | `UNAVAILABLE`        | retry                                                                  |
| index disabled by config, unknown `shieldedProtocol`, unknown method (`GetBlockNullifiers`, `Ping`) | `UNIMPLEMENTED`      | never retry                                                            |
| height, hash or txid not in the chain, or above the answering index's tip                           | `NOT_FOUND`          | ask for something else                                                 |
| `GetBlockRange` reaching past the compact-block index's tip (after the blocks up to it)             | `OUT_OF_RANGE`       | ask again once the tip moves                                           |
| the request's own `grpc-timeout` passed                                                             | `DEADLINE_EXCEEDED`  | allow longer or ask for less                                           |
| malformed request, height past the protocol ceiling, unparseable or foreign-network address         | `INVALID_ARGUMENT`   | fix the request                                                        |
| request body over its cap (64 KiB, or 2 MB + 1 KiB for `SendTransaction`)                           | `RESOURCE_EXHAUSTED` | send less per call                                                     |
| addresses with more receives than one request may walk (`serve.max_address_rows`)                   | `RESOURCE_EXHAUSTED` | fewer addresses per call; one huge address is not a light-wallet query |
| request body not complete within 30 s                                                               | `DEADLINE_EXCEEDED`  | resend                                                                 |
| stored record will not walk                                                                         | `INTERNAL`           | the server is broken                                                   |

## Pool filtering

`BlockRange.poolTypes` selects which pools the returned `CompactBlock`s carry.
An empty list means every shielded pool (`SAPLING`, `ORCHARD`, `IRONWOOD`,
defined once by `Pools::default`), and a non-empty list is served exactly. A
value that names no pool (`POOL_TYPE_INVALID` or an unknown number) is
`INVALID_ARGUMENT`, never silently dropped. `GetMempoolTx` applies the same
rules. `GetBlock` differs: `BlockID` has no `poolTypes`, so a single block comes
back with every pool, including `vin` and `vout`. lightwalletd makes the same
split. `GetBlockRangeNullifiers` ignores a `TRANSPARENT` member, as the proto
requires.

Zaino never silently prunes a requested pool. Records are stored with every pool
and pruned on read, so any requested subset is answerable.

## Known gaps

- `GetTransaction` answers only `TxFilter`'s `hash` arm. The `(block, index)`
  positional arm is `INVALID_ARGUMENT`.
- `GetBlockNullifiers` (deprecated) and `Ping` (testing only) are not in the
  vendored `service.proto`, so both are `UNIMPLEMENTED`. `zingo-netutils` wraps
  both, but nothing in zingolib (pepper-sync included) calls them, and neither
  does the Android SDK ([client requirements](./client-requirements.md)).
- `CompactTx.fee` is filled, unlike lightwalletd. In a mined block the
  utxo-set index resolves the values of the outputs each transaction
  spends. In `GetMempoolTx` it is the fee a trusted validator listed, or the one
  Zaino computed for a peer-quorum admission. It is 0 for a coinbase, for our
  own send before anything lists it, and for a fee of 2^32 zatoshis or more,
  since the field is a `uint32` with no presence bit.
