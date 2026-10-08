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
[design/boundaries.md](./design/boundaries.md): derived answers come from a
Zaino index (never the validator), and writes and point lookups of primary
consensus objects are forwarded to the validator.

| Method | Answered by |
| -------------------------------------------------------- | ----------------------------------------------------------------------------- |
| `GetLatestBlock`, `GetBlock`, `GetBlockRange` | compact-block index |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` | tree-state index |
| `GetAddressUtxos`, `GetAddressUtxosStream` | transparent-address index |
| `GetTaddressBalance`, `GetTaddressBalanceStream` | transparent-address index |
| `GetTaddressTransactions` | transparent-address index for the `{height, txid}` set, the validator for the bytes |
| `GetTransaction` | validator |
| `SendTransaction` | [chain view](./design/chainview.md) submission: one random entry per attempt |
| `GetMempoolTx`, `GetMempoolStream` | chain view (listed by any trusted validator, or our own broadcast) |
| `GetLightdInfo` | chain view (validators' last poll) + the served snapshot tip |
| `GetBlockRangeNullifiers` *(deprecated, TODO: REMOVE)* | compact-block index, re-projected to nullifiers |
| `GetTaddressTxids` *(deprecated, TODO: REMOVE)* | as `GetTaddressTransactions` |

"The validator" here is the traffic balancer (`zaino-traffic`) over every
`[[trusted_validators]]` entry: lowest `priority` tier first, the cheaper of two
within it, a slow answer hedged and an absent one asked of the next until one
answers (a lagging validator may lack a just-mined transaction); `NOT_FOUND` only
when every one asked said absent. The mempool methods read
every trusted validator's listing. The two deprecated rows are served
only because pepper-sync calls them (see
[client-requirements.md](./client-requirements.md)).

A `BlockID` that carries a hash, in `GetBlock` or `GetTreeState`, is resolved to
a height through the block-hash index. A hash wins over a height when both are
given, because a hash names one block across a reorg and a height does not.

Every index can be disabled (at least one stays on); compact-block's fee index,
value-balance (every `CompactTx.fee`), runs with it and has no switch of its
own. A disabled index's methods answer `UNIMPLEMENTED` (for the block-hash
index, only the by-hash form of `GetBlock` and `GetTreeState`). Nothing falls
back to the validator.

Every index method reads one snapshot of every index, pinned for the request or
stream, and answers at heights at or below its tip: `GetLatestBlock` is that
tip, and `GetBlockRange`, `GetTreeState` and the address methods never answer
past it, so they agree with each other within a request.

`GetLightdInfo` takes `chainName` from config (lightwalletd's `main`, `test` or
`regtest`) and `saplingActivationHeight`, `consensusBranchId`, `upgradeName`
and `upgradeHeight` (the next pending upgrade), and `estimatedHeight` from the
validator's `getblockchaininfo`, which it reuses for up to one second because
every wallet polls it. `blockHeight` is the served snapshot tip (0 before the
first), not the validator's, because a wallet gates its sync on it. `lightwalletProtocolVersion`
is the release of the vendored protos (currently `v0.5.0`), which pepper-sync
requires before it syncs ([client-requirements.md](./client-requirements.md)).
With the validator unreachable the call is `UNAVAILABLE`. We never substitute a
stand-in branch, schedule or tip.

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

`GetBlockRange` reaching past the served tip streams every block up to the tip,
then ends with `OUT_OF_RANGE` in its trailers, never an early `OK` (the iOS
downloader waits forever after one). A range whose first height is past the tip
is `OUT_OF_RANGE` before any block.

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
deadline, and `[grpc] stall_timeout_secs` bounds a stream that makes no progress
(see [running.md](./running.md#stream-time-limits)).

## Status codes a client must distinguish

| Condition | Code | Read it as |
| ------------------------------------------------------------------------------------------------ | -------------------- | ------------------------------------------------------------ |
| nothing served yet (indexes opening at boot) | `UNAVAILABLE` | back off and retry |
| stream or subscription cap full (`grpc-retry-pushback-ms: 250`) | `UNAVAILABLE` | retry after the hint |
| no verified tip, or no trusted validator holds it (`GetMempoolStream`, `GetMempoolTx`) | `UNAVAILABLE` | back off and retry |
| validator unreachable (`GetTransaction`, `GetLightdInfo`, `SendTransaction`) | `UNAVAILABLE` | back off and retry |
| stream with nothing to send for `stall_timeout_secs` (not `GetMempoolStream`) | `UNAVAILABLE` | retry |
| index disabled by config, unknown `shieldedProtocol`, unknown method (`GetBlockNullifiers`, `Ping`) | `UNIMPLEMENTED` | never retry |
| height, hash or txid not in the chain, or above the served tip | `NOT_FOUND` | ask for something else |
| `GetBlockRange` reaching past the served tip (after the blocks up to it) | `OUT_OF_RANGE` | ask again once the tip moves |
| the request's own `grpc-timeout` passed | `DEADLINE_EXCEEDED` | allow longer or ask for less |
| bad or oversized range, unparseable or foreign-network address | `INVALID_ARGUMENT` | fix the request |
| request body over its cap (64 KiB, or 2 MB + 1 KiB for `SendTransaction`) | `RESOURCE_EXHAUSTED` | send less per call |
| addresses with more receives than one request may walk (`serve.max_address_rows`) | `RESOURCE_EXHAUSTED` | fewer addresses per call; one huge address is not a light-wallet query |
| request body not complete within 30 s | `DEADLINE_EXCEEDED` | resend |
| stored record will not walk | `INTERNAL` | the server is broken |

While Zaino syncs, the served tip trails the chain: every index answers at it,
as lightwalletd answers at what it has ingested, and `GetLatestBlock` /
`GetLightdInfo.blockHeight` report it, so a wallet gating on them never asks
past what is served. A range ending past the served tip streams up to it, then
ends `OUT_OF_RANGE`. Before
the first snapshot (indexes opening at boot) every index method is
`UNAVAILABLE`.

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
  value-balance index resolves the values of the outputs each transaction
  spends. In `GetMempoolTx` it is the fee the validator listed. It is 0 for a
  coinbase, for our own broadcast before any validator lists it, and for a fee
  of 2^32 zatoshis or more, since the field is a `uint32` with no presence bit
  ([design/boundaries.md](./design/boundaries.md)).
