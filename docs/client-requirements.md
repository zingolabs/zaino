# What the clients actually call

An audit, at the source, of the two sync engines that matter:

- **librustzcash**: `zcash_client_backend/src/sync.rs`, the only RPC caller in
  that workspace. Backs Zashi and zallet.
- **pepper-sync**: `zingolib/pepper-sync`, all traffic through
  `client/fetch.rs`. Backs zingolib and Zingo mobile.

Line references point into the right function at the revision audited; they are
not stable coordinates.

Where each method is answered from is [design/boundaries.md](./design/boundaries.md).
This is the other half: what clients ask for, how often, and where answering
slightly wrong is worse than not answering.

## The call matrix

| RPC                                            | librustzcash                                                                  | pepper-sync                                 |
| ---------------------------------------------- | ----------------------------------------------------------------------------- | ------------------------------------------- |
| `GetLatestBlock`                               | every loop iteration                                                          | once per session                            |
| `GetBlockRange`                                | per batch, dominant                                                           | per scan range, dominant                    |
| `GetBlock`                                     | not called                                                                    | reorg check + backfill                      |
| `GetTreeState`                                 | **1:1 with every `GetBlockRange`**, at `scan_range.start - 1` (`sync.rs:381`) | birthday + fallbacks                        |
| `GetSubtreeRoots`                              | ×3 pools at startup, incl. ironwood                                           | ×3 pools per session, ×2 passes             |
| `GetTransaction`                               | not called                                                                    | per scan target                             |
| `GetTaddressTransactions`                      | not called                                                                    | not called (replaces `GetTaddressTxids`)    |
| `GetTaddressTxids` *(deprecated alias)*        | not called                                                                    | per known t-address + gap-limit discovery   |
| `GetBlockRangeNullifiers` *(deprecated alias)* | not called                                                                    | per `ScannedWithoutMapping` range           |
| `GetAddressUtxosStream`                        | per account per iteration                                                     | dead code                                   |
| `GetMempoolStream`                             | not called                                                                    | background task, retries forever            |
| `SendTransaction`                              | the embedder's job                                                            | zingolib                                    |
| `GetLightdInfo`                                | **never called**                                                              | zingolib server-select only                 |

`GetTreeState` being 1:1 with `GetBlockRange` is the single most important row:
it is the highest-volume method after block fetch itself, and a librustzcash
wallet cannot complete one sync batch without it.

### Two deprecated methods, served until pepper-sync moves off them

`GetBlockRangeNullifiers` and `GetTaddressTxids` are served as deprecated
aliases, **marked TODO: REMOVE** at every site. pepper-sync calls both on every
sync and treats any non-OK status as a fatal `SyncError`, so without them no
pepper-sync wallet syncs against Zaino. They go when pepper-sync calls the
replacements, and should not outlive that:

- **Nullifiers**: superseded by `GetBlockRange` + shielded `poolTypes`, which
  returns a superset (sapling outputs and full action data as well as
  nullifiers). A bandwidth increase on a re-scan path, not a behaviour change.
  It is also **not** expressible as a `poolTypes` subset: the nullifier shape
  drops `chainMetadata`, drops sapling *outputs* while keeping sapling
  *spends*, and descends **inside** each action to keep only the nullifier — a
  third nesting level the pool walk deliberately does not do. Serving it would
  be a second, separately-maintained projection of the same records, and two
  projections of one truth can disagree.
- **`GetTaddressTxids`**: the proto both misnames and deprecates it — *"this
  function is misnamed, it returns complete `RawTransaction` values, not TxIds"*
  and *"please use GetTaddressTransactions instead"*.

## Correctness landmines

**Ironwood tree state must be served correctly or wallets silently mis-sync.**
Both clients map an empty `TreeState.{orchard,ironwood}_tree` to an empty
commitment tree with no error and no warning
(`zcash_client_backend/src/proto.rs:404,420-444`; `pepper-sync/src/witness.rs`),
which is indistinguishable from a genuinely pre-activation height. A post-NU6.3
server that omits the field produces a *wrong* ironwood tree in the wallet
rather than a diagnosable failure. Same tolerance on ironwood
`GetSubtreeRoots`, swallowed to `Vec::new()`. So the tree-state index always
emits all three fields, always as the serialization of the real tree (`000000`
for a genuinely empty one), and never `""` above a pool's activation.

**The wire format is a `CommitmentTree`, not a frontier.**
`TreeState.{sapling,orchard,ironwood}_tree` is hex of
`zcash_primitives::merkle_tree::write_commitment_tree`; both clients parse it
with `read_commitment_tree` and then `.to_frontier()`.

**Neither client sends a block hash.** Both pass `hash: vec![]` to
`GetTreeState`. `BlockID::Hash` is a completeness nicety, not a load path.

**`GetSubtreeRoots` costs one extra empty round trip.** For an unbounded request
pepper-sync does not trust a clean stream end; it re-requests from the current
index and stops only when a pass returns zero roots. Six calls per session, not
three.

**`GetBlockRange` has no client-side size limit.** pepper-sync requests an
entire scan range in one call, and scan ranges are derived from subtree
boundaries — so a historic range can be **2^16 = 65536 blocks** in one streaming
call. Its flow control is client-side and mid-stream: it counts outputs and
splits the scan *task* without cancelling the RPC. Any server-side maximum below
65536 makes Zaino unusable for zingolib, and because the client retries
unboundedly on a stream error it presents as a hang rather than a failure. This
is why `serve.max_block_range` defaults to 131072 and why exceeding it is an
error rather than a short answer — a wallet given fewer blocks than it asked for
reads that as the end of the chain.

**Do not implement `GetAddressUtxos` paging naively.** lightwallet-protocol
issue #33 records that `startHeight` is not a unique resume key, so paging can
stall. It is latent only because no current client pages.

**There is no version handshake anywhere.** Neither client reads
`lightwalletProtocolVersion`: librustzcash has no reader for the field, and
pepper-sync never calls `GetLightdInfo`. The proto's instruction that clients
*MUST* verify server capability before setting `poolTypes` has no implementation
on either side.

## Transparent: the path everyone uses, and the path nobody uses yet

Transparent sync today leaks the wallet. pepper-sync discovers transparent funds
by walking the BIP-44 derivation path and sending **each derived address to the
server**, one per round trip, until `gap_limit` consecutive empties
(`pepper-sync/src/sync/transparent.rs:125-154`) — 20 round trips minimum for a
default single-account wallet, 60 in recovery, strictly serial, and re-paid
**every session**, because trailing gap addresses are truncated rather than
persisted. The server therefore learns every address a wallet derives,
including unused ones past the last used, which are used, and the derivation
order that links them all to one wallet.

The protocol defines the private alternative: request the `TRANSPARENT` pool in
`BlockRange.poolTypes` and match scripts locally, disclosing nothing
(lightwallet-protocol v0.4.0, PR #1; lightwalletd PR #534).

**No client consumes it yet, for two independent reasons.** Both send
`pool_types: vec![]`, and librustzcash's compact scanner hardcodes the
transparent-output argument to `vec![]` and gates the push on
`has_sapling || has_orchard || has_ironwood` with no transparent term
(`scanning/compact.rs:526,529`) — so a purely transparent transaction is dropped
**entirely**, not merely stripped of its transparent fields. Tracked as
librustzcash #2187 and #2395.

Zaino serves the private path, and it is cheap: `CompactTxIn`
is `{prevoutTxid, prevoutIndex}` with **no prevout value**, a deliberate
omission so that servers need no UTXO-value lookup. The consequence is that
compact blocks alone cannot yield a transparent *balance* — that needs prior
outputs — which is why the address RPCs do not go away even once the private
path lands.

Two details copied from lightwalletd's behaviour rather than its schema:
coinbase `vin` is omitted (a client detects coinbase by `CompactTx.index == 0`),
and a coinbase's `fee` is left unset. Unlike lightwalletd, every other mined
transaction's `fee` is filled.

One hard operational rule falls out:

> **Never silently prune a requested `TRANSPARENT`.** If Zaino cannot serve it,
> return an error. Silent pruning costs a user their privacy with nothing in the
> logs.

## The contract is the proto, not a ZIP

ZIP-307 is the only light-client sync ZIP and it covers Sapling only — no
mention of transparent, never extended even to Orchard. ZIP-314, "Privacy
upgrades to the Zcash light client protocol", is `Reserved` with an empty body.
So `zcash/lightwallet-protocol`'s `.proto` comments and CHANGELOG are the
normative source, and `LightdInfo.lightwalletProtocolVersion` is how a server
states what it supports.
