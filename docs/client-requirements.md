# What the clients actually call

What the two wallet sync engines ask for, how often, and where answering slightly wrong is worse
than not answering. Read at the source; references name a function or file, not a line.

- **librustzcash**: `zcash_client_backend/src/sync.rs`, the only RPC caller in that workspace.
  It backs Zashi and zallet.
- **pepper-sync**: `zingolib/pepper-sync`, all traffic through `client/fetch.rs`. It backs
  zingolib and Zingo mobile.

Where Zaino answers each method from: [rpc_api.md](./rpc_api.md).

## The call matrix

| RPC                                            | librustzcash                                                                  | pepper-sync                                  |
| ---------------------------------------------- | ----------------------------------------------------------------------------- | -------------------------------------------- |
| `GetLatestBlock`                               | every loop iteration                                                          | once per session                             |
| `GetBlockRange`                                | per batch, dominant                                                           | per scan range, dominant                     |
| `GetBlock`                                     | not called                                                                    | reorg check and backfill                     |
| `GetTreeState`                                 | **1:1 with every `GetBlockRange`**, at `scan_range.start - 1` (`download_chain_state`) | birthday and fallbacks                       |
| `GetSubtreeRoots`                              | three pools at startup, ironwood included                                     | three pools per session, two passes each     |
| `GetTransaction`                               | not called                                                                    | per scan target                              |
| `GetTaddressTransactions`                      | not called                                                                    | not called (it uses `GetTaddressTxids`)      |
| `GetTaddressTxids` *(deprecated alias)*        | not called                                                                    | per known t-address, and gap-limit discovery |
| `GetBlockRangeNullifiers` *(deprecated alias)* | not called                                                                    | per `ScannedWithoutMapping` range            |
| `GetAddressUtxosStream`                        | per account per iteration                                                     | dead code                                    |
| `GetMempoolStream`                             | not called                                                                    | background task, retries forever             |
| `SendTransaction`                              | the embedder's job                                                            | zingolib                                     |
| `GetLightdInfo`                                | **never called**                                                              | zingolib server selection only               |

`GetTreeState` is 1:1 with `GetBlockRange`, the highest-volume method after block fetch itself:
a librustzcash wallet cannot complete a single sync batch without it.

### Two deprecated methods, served until pepper-sync moves off them

`GetBlockRangeNullifiers` and `GetTaddressTxids` are deprecated aliases, **marked TODO: REMOVE**
at every site. pepper-sync calls both on every sync and treats any non-OK status as a fatal
`SyncError`, so without them no pepper-sync wallet syncs against Zaino. They go when pepper-sync
calls the replacements.

- `GetBlockRangeNullifiers` is superseded by `GetBlockRange` with shielded `poolTypes`, a
  superset. Its shape is not a `poolTypes` subset (it drops `chainMetadata` and sapling outputs,
  and keeps only each action's nullifier), so Zaino re-projects the stored compact blocks rather
  than keep a second projection that could disagree.
- `GetTaddressTxids` is misnamed and deprecated by the proto itself ("please use
  GetTaddressTransactions instead").

## Correctness landmines

**Ironwood tree state must be served correctly or wallets silently mis-sync.** Both clients map
an empty `TreeState.{orchard,ironwood}_tree` to an empty commitment tree with no error
(`TreeState::orchard_tree` / `ironwood_tree` in `zcash_client_backend/src/proto.rs`;
`pepper-sync/src/witness.rs`), indistinguishable from a pre-activation height; `GetSubtreeRoots`
is swallowed into `Vec::new()` the same way. So Zaino serves each pool from its activation on as
the real tree (`000000` when empty), and `""` only below activation
([tree-state guide](../packages/zaino-index-tree-state/usage.md#serving)).

**The wire format is a `CommitmentTree`, not a frontier.** `TreeState.*_tree` is the hex of
`zcash_primitives::merkle_tree::write_commitment_tree`; both clients parse it with
`read_commitment_tree`, then `.to_frontier()`.

**Neither client sends a block hash.** Both pass `hash: vec![]` to `GetTreeState`.

**`GetSubtreeRoots` costs one extra empty round trip.** pepper-sync re-requests until a pass
returns zero roots, so a session makes six calls, not three.

**`GetBlockRange` has no client-side size limit.** pepper-sync requests a whole scan range in
one call, and scan ranges follow subtree boundaries (2^16 *notes*, not blocks): a mainnet wallet
sync asked for 158,853 blocks in one call. Any server-side maximum breaks zingolib for some
birthday, and because it retries a stream error unboundedly, it presents as a hang. So Zaino has
no range cap, and never answers short: a wallet given fewer blocks than it asked for reads that
as the end of the chain.

**Do not implement `GetAddressUtxos` paging naively.** `startHeight` is not a unique resume key
(lightwallet-protocol issue #33), so paging can stall. No current client pages.

**pepper-sync refuses a server below protocol v0.5.0.** It rejects any
`lightwalletProtocolVersion` below `v0.5.0`, an empty one included
(`check_lightwallet_protocol_version` in `pepper-sync/src/client.rs`). Zaino reports its vendored
protos' release (`zaino_proto::LIGHTWALLET_PROTOCOL_VERSION`), so a new upstream release moves
the reported version with it.

## Transparent: address queries and the private path

pepper-sync discovers transparent funds by sending **each derived address to the server**, one
per round trip, until `gap_limit` consecutive addresses come back empty (`address_discovery` in
`pepper-sync/src/sync/transparent.rs`), and re-pays it every session. The server learns every
address a wallet derives, which are used, and the order that links them to one wallet.

The private alternative: request the `TRANSPARENT` pool in `BlockRange.poolTypes` and match
scripts locally (lightwallet-protocol v0.4.0). Zaino serves it. `CompactTxIn` carries no prevout
value, so compact blocks alone cannot yield a transparent *balance*: the address RPCs stay even
once clients move to the private path. As lightwalletd does, a coinbase's `vin` is omitted (a
client detects it by `CompactTx.index == 0`).

> **Never silently prune a requested `TRANSPARENT`.** If Zaino cannot serve it, return an
> error. Silent pruning costs a user their privacy with nothing in the logs.

## The contract is the proto, not a ZIP

ZIP-307 covers Sapling only and ZIP-314 is `Reserved` with an empty body, so the `.proto`
comments and CHANGELOG in `zcash/lightwallet-protocol` are the normative source, and
`LightdInfo.lightwalletProtocolVersion` is how a server states what it supports.
