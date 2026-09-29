# Zaino's RPC surface

`zainod` serves the lightwalletd-compatible `CompactTxStreamer` gRPC service
defined by the
[LightWallet Protocol](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/service.proto)
(message types in
[compact_formats.proto](https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/compact_formats.proto)),
on `serve.grpc_listen_address` (default `127.0.0.1:8137`) over plaintext
HTTP/2. Zaino serves no JSON-RPC: node queries go to the validator's own
JSON-RPC.

## Where each method is answered

Which side a method falls on is not a per-method judgement — it follows from
[design/boundaries.md](./design/boundaries.md). Derived answers come from a
Zaino index and fail while that index is building; writes and point lookups of
primary consensus objects are forwarded to the validator.

| Method | Answered by |
|---|---|
| `GetLatestBlock`, `GetBlock`, `GetBlockRange` | compact-block index |
| `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots` | tree-state index (`GetTreeState` by hash: located via the compact-block index) |
| `GetAddressUtxos`, `GetAddressUtxosStream` | transparent-address index |
| `GetTaddressBalance`, `GetTaddressBalanceStream` | transparent-address index |
| `GetTaddressTransactions` | transparent-address index (the `{height, txid}` set) + validator (the bytes) |
| `GetTransaction` | validator |
| `SendTransaction` | relayed to every validator in the chain view |
| `GetMempoolTx`, `GetMempoolStream` | chain view (quorum mempool) |
| `GetLightdInfo` | validator (network view) + compact-block index (served height) |
| `GetBlockRangeNullifiers` *(deprecated, TODO: REMOVE)* | compact-block index, re-projected to nullifiers |
| `GetTaddressTxids` *(deprecated, TODO: REMOVE)* | as `GetTaddressTransactions` |

The two deprecated rows are served only because pepper-sync calls them (see
[client-requirements.md](./client-requirements.md)).

`GetLightdInfo` takes `chainName` from config (lightwalletd's `main`/`test`/
`regtest`) and `saplingActivationHeight`, `consensusBranchId`,
`upgradeName`/`upgradeHeight` (next pending upgrade) and `estimatedHeight` from
the validator's `getblockchaininfo`, reused for up to one second (every wallet
polls it). With the validator unreachable it is `UNAVAILABLE`: no stand-in
branch, schedule or tip.

## Status codes a client must distinguish

| Condition | Code | Read it as |
|---|---|---|
| index still syncing | `UNAVAILABLE` | back off and retry |
| stream or subscription cap full (`grpc-retry-pushback-ms: 250`) | `UNAVAILABLE` | retry after the hint |
| validators below quorum (`GetMempoolStream`, `GetMempoolTx`) | `UNAVAILABLE` | back off and retry |
| index disabled by config | `UNIMPLEMENTED` | never retry |
| height or txid not in the chain | `NOT_FOUND` | ask for something else |
| bad range, unparseable or foreign-network address | `INVALID_ARGUMENT` | fix the request |
| request body over its cap (64 KiB; `SendTransaction` 2 MB + 1 KiB) | `RESOURCE_EXHAUSTED` | send less per call |
| addresses with more receives than one request may walk (`serve.max_address_rows`) | `RESOURCE_EXHAUSTED` | fewer addresses per call; a single huge address is not a light-wallet query |
| request body not complete within 30 s | `DEADLINE_EXCEEDED` | resend |
| stored record will not walk | `INTERNAL` | the server is broken |

A syncing index refuses **every** request with one error, carrying no height and
no progress: a client cannot tell "no such block" from "not indexed yet". The
one exception is `GetTreeState` at a height the tree-state index has already
committed: that answer is final, so it is served while the index is still
building.

## Pool filtering

`BlockRange.poolTypes` selects which pools the returned `CompactBlock`s carry.
An empty list means every shielded pool (`SAPLING`, `ORCHARD`, `IRONWOOD`,
defined once by `Pools::default`); a non-empty list is served exactly. A value
that names no pool (`POOL_TYPE_INVALID` or an unknown number) is
`INVALID_ARGUMENT`, never silently dropped. `GetMempoolTx` applies the same
rules. `GetBlock` differs: a single block comes back with all pools, including
`vin`/`vout`. lightwalletd makes the same split.

Zaino never silently prunes a requested pool: records are stored with every pool
and pruned on read, so any requested subset is answerable.

## Known gaps

- `LightdInfo.lightwalletProtocolVersion` is left unset. The proto requires a
  client to feature-detect there before requesting non-default `poolTypes`, so a
  spec-following client cannot use the transparent compact blocks Zaino serves.
  No current client reads the field (see
  [client-requirements.md](./client-requirements.md)).
- `GetTransaction` answers only `TxFilter`'s `hash` arm; the `(block, index)`
  positional arm is `INVALID_ARGUMENT`.
- `CompactTx.fee` is filled, unlike lightwalletd: the value-balance index
  resolves the values of outputs spent in prior blocks. It is 0 for a coinbase
  and for a fee of 2^32 zatoshis or more ([design/boundaries.md](./design/boundaries.md)).
