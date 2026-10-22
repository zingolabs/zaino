# Zaino's RPC surface

`zainod` serves the lightwalletd-compatible `CompactTxStreamer` gRPC service defined by the
[LightWallet Protocol](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/service.proto)
on `serve.grpc_listen_address` (default `127.0.0.1:8137`), HTTP/2, plaintext unless
`[serve.tls]` is set. Zaino serves no JSON-RPC: node queries go to the validator's own.

## Where each method is answered

Derived answers come from a Zaino index, never the validator; consensus objects a validator
already stores are asked of it ([architecture §1](./architecture.md#1-who-owns-what)).

| Method                                                  | Answered by                                                                                                                                    |
| ------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `GetLatestBlock`                                        | the served tip (no index read)                                                                                                                 |
| `GetBlock`, `GetBlockRange`                             | compact-block index                                                                                                                            |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` | tree-state index                                                                                                                               |
| `GetAddressUtxos`, `GetAddressUtxosStream`              | transparent-address index                                                                                                                      |
| `GetTaddressBalance`, `GetTaddressBalanceStream`        | transparent-address index                                                                                                                      |
| `GetTaddressTransactions`                               | transparent-address index for the `{height, txid}` set, the bytes as `GetTransaction`'s                                                        |
| `GetTransaction`                                        | validator; none answering: the served mempool's bytes, else the block the txid-location index names, from p2p peers, checked and the transaction cut out |
| `GetStandardFee`                                        | computed (no validator call): the ZIP 317 marginal fee for the block after best, zatoshis per logical action (`zaino_primitives::protocol::standard_fee`: zebrad's rule), estimator version 0 |
| `SendTransaction`                                       | chain view [submission](./architecture.md#6-submission): a trusted `checkadmission` answers, then relayed through the peers                    |
| `GetMempoolTx`, `GetMempoolStream`                      | chain view [mempool](./architecture.md#5-the-mempool-view): listed by a trusted validator, admitted by a peer quorum, or our own accepted send |
| `GetLightdInfo`                                         | the served `GrpcSnapshot`: its tip, the verified best, the upgrade schedule (no validator call)                                                |
| `GetBlockRangeNullifiers` *(deprecated, TODO: REMOVE)*  | compact-block index, re-projected to nullifiers                                                                                                |
| `GetTaddressTxids` *(deprecated, TODO: REMOVE)*         | as `GetTaddressTransactions`                                                                                                                   |

- "The validator" is the traffic balancer over every `[[trusted_validators]]` entry. An absent
  answer is asked of the next validator (a lagging one may lack a just-mined transaction):
  `NOT_FOUND` only when every one asked said absent.
- Why the deprecated rows exist:
  [client-requirements.md](./client-requirements.md#two-deprecated-methods-served-until-pepper-sync-moves-off-them).
- Every index can be disabled (at least one stays on); utxo-set, which fills every
  `CompactTx.fee`, runs with compact-block. A disabled index's methods are `UNIMPLEMENTED`; for
  block-hash that is only the by-hash form of `GetBlock` and `GetTreeState`.
- A method naming no height (`GetLatestBlock`, `GetLatestTreeState`, `GetSubtreeRoots`,
  `GetLightdInfo.blockHeight`, the address methods) answers at the served tip, which every
  enabled index holds, so a wallet gating on `GetLatestBlock` never asks past what every index
  holds. A method naming a height (`GetBlock`, `GetBlockRange`, `GetTreeState`) answers up to its
  own index's tip, which during sync may be above the served tip.
- A `BlockID` carrying a hash is resolved through the block-hash index. A hash wins over a
  height when both are given: a hash names one block across a reorg.
- No stream has a total-duration cap ([stream time limits](./running.md#stream-time-limits)).

`GetLightdInfo` never calls a validator, so it answers while no trusted validator vouches too.
`blockHeight` is the served tip, `estimatedHeight` the verified best, `chainName` the configured
network; the branch and next upgrade come from the upgrade schedule at the served tip.
`lightwalletProtocolVersion` is the vendored protos' release (the newest heading of
`packages/zaino-proto/lightwallet-protocol/CHANGELOG.md`).

`GetTransaction` answers a mined transaction with its height, and an unmined or orphaned one
with height 0, the wire's only "no height" value.

`SendTransaction` reports a rejection in the `SendResponse` itself, as lightwalletd does: gRPC
`OK`, `errorCode` = the validator's own JSON-RPC code and `errorMessage` = its message verbatim
(`-22` for bytes that do not parse, `-25` for the expiry and consensus-branch prechecks Zaino
runs itself, as zebrad would answer them). A wallet has to tell "the network said no" from "the
network is unreachable", and zingolib matches the message text ("transaction already exists in
mempool", …). zebrad's `-1` answers that judge the transaction count as rejections; its other
`-1`s (queue full, mempool disabled) do not. The call fails with a status only when no validator
accepted and at least one could not be reached. An accepted transaction answers `errorCode` 0
and the quoted display-order txid.

`GetBlockRange` reaching past the compact-block index's tip streams every block up to it, then
ends with `OUT_OF_RANGE` in its trailers, never an early `OK` (the iOS downloader waits forever
after one). A range whose first height is past it is `OUT_OF_RANGE` before any block.

`GetTreeState` answers exactly the height asked for, with the block hash in display order. Each
tree is `""` below its pool's activation and the serialized tree from activation on, `000000`
while empty (pepper-sync rejects `""` for an active pool).

`GetSubtreeRoots` answers each request whole or refuses it before the first byte. A `startIndex`
past the last root is an empty `OK` stream, and `maxEntries` 0 means every root. An unknown
`shieldedProtocol` is `UNIMPLEMENTED`, the one refusal the Android SDK survives without dropping
its fast sync.

## Status codes a client must distinguish

| Condition                                                                                           | Code                 | Read it as                                                             |
| --------------------------------------------------------------------------------------------------- | -------------------- | ---------------------------------------------------------------------- |
| nothing served yet (booting, indexes opening)                                                       | `UNAVAILABLE`        | back off and retry                                                     |
| work-stream or subscription cap full (`grpc-retry-pushback-ms: 250`)                                | `UNAVAILABLE`        | retry after the hint                                                   |
| no verified tip, or neither a trusted validator holding it nor a peer quorum (mempool methods)      | `UNAVAILABLE`        | back off and retry                                                     |
| no validator answering, txid neither in the served mempool nor in a peer's block (tx bytes methods) | `UNAVAILABLE`        | back off and retry (absence unprovable without a validator)            |
| `SendTransaction` neither admitted by a validator nor listed by a peer quorum                       | `UNAVAILABLE`        | back off and retry                                                     |
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

`BlockRange.poolTypes` selects which pools the returned `CompactBlock`s carry. An empty list
means the [unfiltered pool set](../CONTEXT.md#chains-and-pools); a non-empty list is served
exactly. A value that names no pool (`POOL_TYPE_INVALID` or an unknown number) is
`INVALID_ARGUMENT`, never silently dropped. `GetMempoolTx` applies the same rules. `GetBlock`
has no `poolTypes`, so a single block comes back with every pool, `vin` and `vout` included, as
lightwalletd does. `GetBlockRangeNullifiers` ignores a `TRANSPARENT` member, as the proto
requires. Records are stored with every pool and pruned on read, so any requested subset is
answerable and none is silently pruned.

## Known gaps

- `GetTransaction` answers only `TxFilter`'s `hash` arm; the `(block, index)` arm is
  `INVALID_ARGUMENT`.
- `GetBlockNullifiers` (deprecated) and `Ping` (testing only) are not in the vendored
  `service.proto`, so both are `UNIMPLEMENTED`; no client calls them
  ([client requirements](./client-requirements.md)).
- `CompactTx.fee` is filled, unlike lightwalletd: in a mined block from the utxo-set index, in
  `GetMempoolTx` the fee a trusted validator listed or the one Zaino computed for a peer-quorum
  admission. It is 0 for a coinbase, for our own send before anything lists it, and for a fee of
  2^32 zatoshis or more (the field is a `uint32` with no presence bit).
