# zaino-chainview: one view over many validators

`zaino-chainview` is Zaino's view of the chain tip and the mempool across every
validator the operator configures. A wallet talking to one node cannot tell "my
transaction is stuck" from "my transaction has propagated", or "I am behind"
from "the chain forked". By polling N validators and folding their answers
together, every answer the view gives carries how many nodes agree, which is the
one thing a client cannot compute for itself.

Membership is `[source]` followed by every `[[chainview_peers]]` entry. With no
peers configured the view is a single validator with a threshold of one, so the
wiring is the same in every deployment.

The view has three consumers. Its quorum tip is what block sync follows: the
producer bulk-fetches up to the finalized boundary below it (spread across every
configured validator unless `fetch.primary_validator` names one), then advances
the non-finalized state each time the quorum tip moves. Its mempool answers `GetMempoolTx` and
`GetMempoolStream`. Its broadcast answers `SendTransaction`. `GetTransaction` and
`GetLightdInfo` do not go through the view; they read `[source]` alone. The view
speaks JSON-RPC only (§6). Cadence constants and the Rust API are in the crate's
[usage guide](../../packages/zaino-chainview/usage.md).

## 1. Membership is configured, never discovered

The operator supplies the endpoint set, as an explicit list or from service
discovery inside a trust domain they control. We never derive it from the chain,
because **a quorum over a discovered set is not a quorum**. Quorum security rests
on the set being curated, and an adversary who can inject endpoints outvotes the
honest ones. The Zcash p2p network is not Sybil-resistant (proof-of-work protects
the chain, not a peer table), so `getpeerinfo` is the wrong source for
membership.

It is a good source of telemetry. Each poller reads its validator's
`getpeerinfo` every 60 seconds into `ValidatorMetadata::peers`, and
cross-referencing those peer sets says whether the configured validators are
partitioned (no peers in common) or plausibly eclipsed (all sharing one small
set). That never decides membership, so it costs nothing in trust.

## 2. Two layers

```text
  EndpointPoller × N          one validator each: poll, diff, report a delta
        │
        ▼
  ChainViewCore               folds deltas into one ChainViewSnapshot, published via ArcSwap
        │
        ▼
  ChainViewSubscriber         readers pin one snapshot per request or stream
```

Each validator gets its own `EndpointPoller`, which owns its interval, backoff
and failure count. A slow or flaky validator therefore degrades alone: while it
is on its backoff ladder it stops voting, and the rest of the view carries on.
The poller diffs each listing against its own previous one and reports only the
change (added, removed, and its tip), so folding a report costs `O(change)`
rather than `O(endpoints × mempool)` per tick.

The fold publishes a new snapshot after every report, cheaply because the
collections are `imbl`. A reader pins one snapshot per request or stream, so a
fold that lands mid-response cannot splice two views into one answer.

Giving up is different from degrading. Ten consecutive failures, or a validator
that reports it has no mempool at all, ejects the endpoint: its sightings and its
vote are retracted and its poller returns an error. `zainod` treats that error
as fatal and exits, so a validator that stays unreachable through the whole
backoff ladder stops the daemon.

## 3. The snapshot

```rust
pub struct ChainViewSnapshot {
    tip: Option<QuorumTip>,               // None below quorum
    epoch: u64,                           // bumped on every tip change
    mempool: OrdMap<TransactionId, Sighting>,
    arrivals: Vector<TransactionId>,      // became servable this epoch, in order
    endpoints: Vector<ValidatorMetadata>,
    quorum: Quorum,
}

struct Sighting {
    seen_at: EndpointSet,                 // bitset over `endpoints`
    ours: bool,                           // relayed by us
    raw: Bytes,
    fee: Option<Zatoshis>,
}
```

`seen_at` is a **bitset, not a count**. It costs the same and it answers *which*
nodes have the transaction, which is what makes partition and eclipse analysis
possible. `QuorumTip::agreed_by` is the same bitset for the tip. Its width caps
the configured set at 64 endpoints.

The fee is the first one any validator listed through `getrawmempool true`
(every validator lists the same one), and it is `None` only for our own
broadcast before any validator has listed it.

`ValidatorMetadata` carries what a decision about one endpoint needs:

| Field                | Decision it enables                  |
| -------------------- | ------------------------------------ |
| `tip`, `observed_at` | quorum tip, staleness                |
| `latency` (EWMA)     | routing                              |
| `failures`, `state`  | eject and back off                   |
| `agreement`          | is this node trustworthy *right now* |
| `peers`              | partition and eclipse telemetry      |

Raw bytes are fetched **once**. `getrawmempool` returns ids and bytes cost a
round trip, so a poller that lists a txid the view already holds reports it
without fetching. Paying that round trip per endpoint per transaction would be N
times the work for the same bytes.

## 4. Quorum

The threshold is `⌊N/2⌋ + 1` over the **configured** set, not the responding
set. Majority-of-responding is trivially subvertible: DoS three of five
validators and the remaining two become a "quorum".

The **tip** is the highest block that at least threshold endpoints agree on *by
hash*, never the maximum height, or one node claiming height 999,999 would move
it. An endpoint votes while it is `Live`, and also while it is `CatchingUp`: a
validator behind the network tip reports its mempool as inactive, so its
sightings are retracted, but its tip still counts so block sync can follow it.
An endpoint that is `Pending`, `Degraded`, `Down`, or `Syncing` (the node says it
is not ready) does not vote.

**Fail closed.** Below threshold, `tip` is `None`. The mempool RPCs refuse with
gRPC `UNAVAILABLE`, the same rule as a syncing index, and block sync waits for
quorum to return. Mempool membership requires quorum too, with one exception
(§5).

## 5. Downstream

### Broadcast (`SendTransaction`)

We fan every broadcast out to **every** endpoint. N entry points propagate
faster than one, and one dead node cannot block a send.

The send succeeds if **any** endpoint accepts. A node rejecting what another
accepted usually has a stricter local policy (a fee filter), not a different
idea of validity. Only a unanimous rejection is reported to the wallet as a
rejection, and then it is the real one. If nothing accepted and at least one
endpoint was unreachable, we cannot say the transaction was rejected, so the
call fails with `UNAVAILABLE`.

On acceptance the transaction is marked `ours`.

### Mempool RPCs

`GetMempoolTx` and `GetMempoolStream` serve a transaction when
`seen_at.count() >= threshold` **or** it is `ours`. Without the `ours` exception
a wallet's own transaction would be hidden for the seconds it takes to
propagate, which is the one case wallets care most about, and `SendTransaction`
is the one place that can know it. An `ours` transaction that no validator lists
is dropped the next time the quorum tip moves. Any other transaction leaves the
view only when every endpoint stops listing it, since an unmined transaction
survives the block that did not include it.

`GetMempoolStream` sends a snapshot of the servable mempool and then tails it,
matching lightwalletd. It ends when the quorum tip moves or quorum is lost,
never when the mempool empties, so an empty mempool with no block being mined is
a live, silent stream, not a hang. The snapshot is not optional: pepper-sync
reopens this stream in a loop and has no other way to learn of a transaction
that arrived while it was reconnecting.

### Tails at scale

Every connected pepper-sync wallet holds one mempool stream and reopens it on
every block, so the cost of a tail is multiplied by the number of wallets. We
keep that cost small in three ways.

Each published snapshot carries an **epoch**, bumped on every tip change, and
the epoch's **arrivals**, the txids that became servable during it, in order. A
tail pins the snapshot it opened on and keeps a cursor into the arrivals. It
wakes only when an arrival lands or the epoch moves, never on propagation churn,
and the only state it owns is the set of txids it delivered after its snapshot.
Finally, the serving layer renders a snapshot once per published view and shares
the bytes across every tail anchored there, so a block that reconnects every
wallet at once costs one render.

### Propagation data

`RawTransaction` is `{ data, height }`, with nowhere to put propagation data.
Each `Sighting` records *which* endpoints list the transaction. A wallet
watching its transaction go from 1/5 to 4/5 would know it is propagating, and
one watching 1/5 hold still would know it is not. Neither is observable from a
single node. No RPC exposes this yet, because the gRPC surface has no field for
it.

## 6. Why not p2p

A p2p connection sees transactions earlier and gives propagation topology
natively, but its transactions are **unvalidated**. An `inv` means "a peer
relayed this", where `getrawmempool` means "this node validated and accepted
it", and serving wire sightings would show a wallet transactions no validator
accepted. Add the cost of pulling Zcash's network stack into an indexer that
holds no consensus code ([boundaries.md](./boundaries.md)), and it is not worth
the roughly one second of poll lag it saves.
