# What the clients actually call

This is an audit, read at the source, of the two wallet sync engines that
matter:

- **librustzcash**: `zcash_client_backend/src/sync.rs`, the only RPC caller in
  that workspace. It backs Zashi and zallet.
- **pepper-sync**: `zingolib/pepper-sync`, with all traffic going through
  `client/fetch.rs`. It backs zingolib and Zingo mobile.

Line references point into the right function at the revision audited. They are
not stable coordinates.

[rpc_api.md](./rpc_api.md) and [design/boundaries.md](./design/boundaries.md)
say where Zaino answers each method from. This document is the other half: what
clients ask for, how often, and where answering slightly wrong is worse than not
answering.

## The call matrix

| RPC                                            | librustzcash                                                                  | pepper-sync                               |
| ---------------------------------------------- | ----------------------------------------------------------------------------- | ----------------------------------------- |
| `GetLatestBlock`                               | every loop iteration                                                          | once per session                          |
| `GetBlockRange`                                | per batch, dominant                                                           | per scan range, dominant                  |
| `GetBlock`                                     | not called                                                                    | reorg check and backfill                  |
| `GetTreeState`                                 | **1:1 with every `GetBlockRange`**, at `scan_range.start - 1` (`sync.rs:381`) | birthday and fallbacks                    |
| `GetSubtreeRoots`                              | three pools at startup, ironwood included                                     | three pools per session, two passes each  |
| `GetTransaction`                               | not called                                                                    | per scan target                           |
| `GetTaddressTransactions`                      | not called                                                                    | not called (it uses `GetTaddressTxids`)   |
| `GetTaddressTxids` *(deprecated alias)*        | not called                                                                    | per known t-address, and gap-limit discovery |
| `GetBlockRangeNullifiers` *(deprecated alias)* | not called                                                                    | per `ScannedWithoutMapping` range         |
| `GetAddressUtxosStream`                        | per account per iteration                                                     | dead code                                 |
| `GetMempoolStream`                             | not called                                                                    | background task, retries forever          |
| `SendTransaction`                              | the embedder's job                                                            | zingolib                                  |
| `GetLightdInfo`                                | **never called**                                                              | zingolib server selection only            |

The most important row is `GetTreeState`. It is 1:1 with `GetBlockRange`, which
makes it the highest-volume method after block fetch itself, and a librustzcash
wallet cannot complete a single sync batch without it.

### Two deprecated methods, served until pepper-sync moves off them

We serve `GetBlockRangeNullifiers` and `GetTaddressTxids` as deprecated aliases,
**marked TODO: REMOVE** at every site. pepper-sync calls both on every sync and
treats any non-OK status as a fatal `SyncError`, so without them no pepper-sync
wallet syncs against Zaino. They go when pepper-sync calls the replacements, and
should not outlive that.

`GetBlockRangeNullifiers` is superseded by `GetBlockRange` with shielded
`poolTypes`, which returns a superset: sapling outputs and full action data as
well as nullifiers. For pepper-sync that is a bandwidth increase on a re-scan
path, not a behaviour change. The nullifier shape is also **not** expressible as
a `poolTypes` subset. It drops `chainMetadata`, drops sapling *outputs* while
keeping sapling *spends*, and descends **inside** each action to keep only the
nullifier, a third nesting level the pool walk deliberately does not do. We
serve it by re-projecting the stored compact blocks rather than maintaining a
second projection of the same records, because two projections of one truth can
disagree.

`GetTaddressTxids` is misnamed and deprecated by the proto itself: *"this
function is misnamed, it returns complete `RawTransaction` values, not TxIds"*,
and *"please use GetTaddressTransactions instead"*.

## Correctness landmines

**Ironwood tree state must be served correctly or wallets silently mis-sync.**
Both clients map an empty `TreeState.{orchard,ironwood}_tree` to an empty
commitment tree with no error and no warning
(`zcash_client_backend/src/proto.rs:404,420-444`; `pepper-sync/src/witness.rs`),
which is indistinguishable from a genuinely pre-activation height. A post-NU6.3
server that omits the field therefore produces a *wrong* ironwood tree in the
wallet rather than a diagnosable failure. `GetSubtreeRoots` has the same
tolerance on ironwood, where the result is swallowed into `Vec::new()`. For this
reason the tree-state index emits all three fields at every height, always as
the serialization of the real tree, which is `000000` for an empty one. It never
emits `""`.

**The wire format is a `CommitmentTree`, not a frontier.**
`TreeState.{sapling,orchard,ironwood}_tree` is the hex of
`zcash_primitives::merkle_tree::write_commitment_tree`. Both clients parse it
with `read_commitment_tree` and then call `.to_frontier()`.

**Neither client sends a block hash.** Both pass `hash: vec![]` to
`GetTreeState`, so `BlockID::Hash` is a completeness nicety, not a load path.

**`GetSubtreeRoots` costs one extra empty round trip.** For an unbounded
request, pepper-sync does not trust a clean end of stream. It re-requests from
the current index and stops only when a pass returns zero roots, so a session
makes six calls, not three.

**`GetBlockRange` has no client-side size limit.** pepper-sync requests an
entire scan range in one call, and scan ranges are derived from subtree
boundaries. A subtree is 2^16 *notes*, not blocks, so in a quiet stretch of the
chain one range spans any number of blocks: a mainnet wallet sync asked for
158,853 in one call. Its flow control is client-side and mid-stream: it counts
outputs and splits the scan *task* without cancelling the RPC. Any server-side
maximum breaks zingolib for some birthday, and because the client retries
unboundedly on a stream error, it presents as a hang rather than a failure. So
Zaino has no range cap (the stream is bounded per 1 MiB window instead), and a
short answer is never an option: a wallet given fewer blocks than it asked for
reads that as the end of the chain.

**Do not implement `GetAddressUtxos` paging naively.** lightwallet-protocol
issue #33 records that `startHeight` is not a unique resume key, so paging can
stall. It is latent only because no current client pages.

**There is no version handshake anywhere.** Neither client reads
`lightwalletProtocolVersion`: librustzcash has no reader for the field, and
pepper-sync never calls `GetLightdInfo`. The proto's instruction that clients
*MUST* verify server capability before setting `poolTypes` has no
implementation on either side.

## Transparent: the path everyone uses, and the path nobody uses yet

Transparent sync today leaks the wallet. pepper-sync discovers transparent funds
by walking the BIP-44 derivation path and sending **each derived address to the
server**, one per round trip, until `gap_limit` consecutive addresses come back
empty (`pepper-sync/src/sync/transparent.rs:125-154`). That is 20 round trips at
minimum for a default single-account wallet and 60 in recovery, strictly serial,
and re-paid **every session**, because the trailing gap addresses are truncated
rather than persisted. The server therefore learns every address a wallet
derives, including the unused ones past the last used one, which of them are
used, and the derivation order that links them all to one wallet.

The protocol defines the private alternative: request the `TRANSPARENT` pool in
`BlockRange.poolTypes` and match scripts locally, disclosing nothing
(lightwallet-protocol v0.4.0, PR #1; lightwalletd PR #534).

**No client consumes it yet, for two independent reasons.** Both clients send
`pool_types: vec![]`. And librustzcash's compact scanner hardcodes the
transparent-output argument to `vec![]` and gates the push on
`has_sapling || has_orchard || has_ironwood` with no transparent term
(`scanning/compact.rs:526,529`), so a purely transparent transaction is dropped
**entirely**, not merely stripped of its transparent fields. This is tracked as
librustzcash #2187 and #2395.

Zaino serves the private path, and it is cheap for us to do so. `CompactTxIn` is
`{prevoutTxid, prevoutIndex}` with **no prevout value**, a deliberate omission
so that servers need no UTXO-value lookup. The consequence is that compact
blocks alone cannot yield a transparent *balance*, which needs the prior
outputs, and that is why the address RPCs will not go away even once clients
move to the private path.

Two details are copied from lightwalletd's behaviour rather than its schema. A
coinbase's `vin` is omitted, since a client detects a coinbase by
`CompactTx.index == 0`, and a coinbase's `fee` is left unset. Unlike
lightwalletd, we fill the `fee` of every other mined transaction.

One hard operational rule falls out of all this:

> **Never silently prune a requested `TRANSPARENT`.** If Zaino cannot serve it,
> return an error. Silent pruning costs a user their privacy with nothing in the
> logs.

## The contract is the proto, not a ZIP

ZIP-307 is the only light-client sync ZIP, and it covers Sapling only. It never
mentions transparent and was never extended even to Orchard. ZIP-314, "Privacy
upgrades to the Zcash light client protocol", is `Reserved` with an empty body.
So the `.proto` comments and CHANGELOG in `zcash/lightwallet-protocol` are the
normative source, and `LightdInfo.lightwalletProtocolVersion` is how a server
states what it supports.
