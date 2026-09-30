# Zaino's RPC surface

`zainod` serves the lightwalletd-compatible `CompactTxStreamer` gRPC service
defined by the
[LightWallet Protocol](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/service.proto)
(message types in
[compact_formats.proto](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/compact_formats.proto)),
on `serve.grpc_listen_address` (default `127.0.0.1:8137`) over plaintext HTTP/2.
Zaino serves no JSON-RPC. Node queries go to the validator's own JSON-RPC.

## Where each method is answered

Which side a method falls on is not a per-method judgement. It follows from
[design/boundaries.md](./design/boundaries.md): derived answers come from a
Zaino index and fail while that index is building, and writes and point lookups
of primary consensus objects are forwarded to the validator.

| Method                                                   | Answered by                                                                   |
| -------------------------------------------------------- | ----------------------------------------------------------------------------- |
| `GetLatestBlock`, `GetBlock`, `GetBlockRange`            | compact-block index                                                           |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots`  | tree-state index                                                              |
| `GetAddressUtxos`, `GetAddressUtxosStream`               | transparent-address index                                                     |
| `GetTaddressBalance`, `GetTaddressBalanceStream`         | transparent-address index                                                     |
| `GetTaddressTransactions`                                | transparent-address index for the `{height, txid}` set, the validator for the bytes |
| `GetTransaction`                                         | validator                                                                     |
| `SendTransaction`                                        | relayed to every validator in the [chain view](./design/chainview.md)         |
| `GetMempoolTx`, `GetMempoolStream`                       | chain view (quorum mempool)                                                   |
| `GetLightdInfo`                                          | validator for the network view, compact-block index for the served height     |
| `GetBlockRangeNullifiers` *(deprecated, TODO: REMOVE)*   | compact-block index, re-projected to nullifiers                               |
| `GetTaddressTxids` *(deprecated, TODO: REMOVE)*          | as `GetTaddressTransactions`                                                  |

"The validator" here is `[source]`. Only `SendTransaction` and the mempool
methods use the other configured validators. The two deprecated rows are served
only because pepper-sync calls them (see
[client-requirements.md](./client-requirements.md)).

A `BlockID` that carries a hash, in `GetBlock` or `GetTreeState`, is resolved to
a height through the block-hash index. A hash wins over a height when both are
given, because a hash names one block across a reorg and a height does not.

The compact-block and value-balance indexes cannot be disabled, since
`GetLightdInfo` and every `CompactTx.fee` depend on them. The tree-state,
transparent-address and block-hash indexes can. A disabled index's methods
answer `UNIMPLEMENTED` (for the block-hash index, only the by-hash form of
`GetBlock` and `GetTreeState`). Nothing falls back to the validator.

`GetLightdInfo` takes `chainName` from config (lightwalletd's `main`, `test` or
`regtest`) and `saplingActivationHeight`, `consensusBranchId`, `upgradeName`
and `upgradeHeight` (the next pending upgrade), and `estimatedHeight` from the
validator's `getblockchaininfo`, which it reuses for up to one second because
every wallet polls it. `blockHeight` is the compact-block index's tip, not the
validator's, because a wallet gates its sync on it. With the validator
unreachable the call is `UNAVAILABLE`. We never substitute a stand-in branch,
schedule or tip.

`GetTransaction` answers a mined transaction with its height, and an unmined or
orphaned one with height 0, the wire's only "no height" value.

`SendTransaction` reports a rejection in the `SendResponse` itself, with
`errorCode` -1 and the validator's message, because a wallet has to tell "the
network said no" from "the network is unreachable". The call only fails with a
status when no validator accepted and at least one could not be reached.

## Status codes a client must distinguish

| Condition                                                                                        | Code                 | Read it as                                                   |
| ------------------------------------------------------------------------------------------------ | -------------------- | ------------------------------------------------------------ |
| index still syncing                                                                              | `UNAVAILABLE`        | back off and retry                                           |
| stream or subscription cap full (`grpc-retry-pushback-ms: 250`)                                  | `UNAVAILABLE`        | retry after the hint                                         |
| validators below quorum (`GetMempoolStream`, `GetMempoolTx`)                                     | `UNAVAILABLE`        | back off and retry                                           |
| validator unreachable (`GetTransaction`, `GetLightdInfo`, `SendTransaction`)                     | `UNAVAILABLE`        | back off and retry                                           |
| index disabled by config                                                                         | `UNIMPLEMENTED`      | never retry                                                  |
| height, hash or txid not in the chain                                                            | `NOT_FOUND`          | ask for something else                                       |
| bad or oversized range, unparseable or foreign-network address                                   | `INVALID_ARGUMENT`   | fix the request                                              |
| request body over its cap (64 KiB, or 2 MB + 1 KiB for `SendTransaction`)                        | `RESOURCE_EXHAUSTED` | send less per call                                           |
| addresses with more receives than one request may walk (`serve.max_address_rows`)               | `RESOURCE_EXHAUSTED` | fewer addresses per call; one huge address is not a light-wallet query |
| request body not complete within 30 s                                                            | `DEADLINE_EXCEEDED`  | resend                                                       |
| stored record will not walk                                                                      | `INTERNAL`           | the server is broken                                         |

A syncing index refuses **every** request with one error, carrying no height and
no progress, so a client cannot tell "no such block" from "not indexed yet". The
one exception is `GetTreeState` at a height the tree-state index has already
committed. That answer is final, so we serve it while the index is still
building.

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

- `LightdInfo.lightwalletProtocolVersion` is left unset. The proto requires a
  client to feature-detect there before requesting non-default `poolTypes`, so a
  spec-following client cannot use the transparent compact blocks Zaino serves.
  No current client reads the field (see
  [client-requirements.md](./client-requirements.md)).
- `GetTransaction` answers only `TxFilter`'s `hash` arm. The `(block, index)`
  positional arm is `INVALID_ARGUMENT`.
- `CompactTx.fee` is filled, unlike lightwalletd. In a mined block the
  value-balance index resolves the values of the outputs each transaction
  spends. In `GetMempoolTx` it is the fee the validator listed. It is 0 for a
  coinbase, for our own broadcast before any validator lists it, and for a fee
  of 2^32 zatoshis or more, since the field is a `uint32` with no presence bit
  ([design/boundaries.md](./design/boundaries.md)).
