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
`getpeerinfo` every 60 seconds into `ValidatorMetadata::peers` (while the
validator is `Live` or `CatchingUp`), and the fold cross-references the live
endpoints' *outbound* peers after every report:

- **Partition**: two live validators share no outbound peer.
- **Eclipse**: the live validators together reach at most
  `ECLIPSE_OUTBOUND_MAX` distinct outbound peers (an isolated node, such as a
  regtest validator with no peers at all, raises nothing).
- **Stale tip**: a live validator's tip trails its own `estimatedheight` by at
  least `STALE_TIP_BLOCKS`. Zebra estimates that height from the tip block's
  time and the target spacing, not from its peers, so the gap says "this node
  stopped advancing", which is what an eclipsed or stalled node looks like.

Inbound peers are left out of the comparison because their addresses carry
ephemeral ports. Each condition is logged once when it rises and once when it
clears, and the raw inputs are exported as `zaino.chainview.*` gauges. None of
it decides membership, a vote, or whether anything is served, so a false alarm
(or an adversary provoking one) costs a log line and nothing in trust. A failed
`getpeerinfo` keeps the last answer and never fails the poll.

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
change (added, removed, and its chain), so folding a report costs `O(change)`
rather than `O(endpoints × mempool)` per tick.

The chain is the validator's tip plus `finalised_depth` ancestors, read with
`getblockheader <height> false` and checked link by link against each child's
`prev_hash` (the hash is recomputed from the header bytes, never taken on
trust). Each walk starts at the reported tip and descends until it joins the
chain held from the previous tick, so steady state costs one header per new
block and the first poll costs `finalised_depth` headers once. A validator that
reorgs between reporting its tip and answering for its headers keeps last
tick's chain: that is a race, not a failure.

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
    agreeing: EndpointSet,                // largest group holding one common block
    epoch: u64,                           // bumped when the tip *block* changes
    mempool: OrdMap<TransactionId, Sighting>,
    arrivals: Vector<TransactionId>,      // became servable this epoch, in order
    endpoints: Vector<ValidatorMetadata>,
    alarms: Alarms,                       // partition / eclipse / stale, edge-logged
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

| Field                     | Decision it enables                                     |
| ------------------------- | ------------------------------------------------------- |
| chain (`tip()`)           | its vote: the tip and every ancestor in its window      |
| `observed_at`             | staleness of the observation                            |
| `estimated_height`        | stale-tip telemetry                                     |
| `latency` (EWMA)          | routing                                                 |
| `failures`, `state`       | eject and back off                                      |
| `agreement`               | `Agreed`, `Ahead`, `Behind`, `Diverged` or `Unknown`    |
| `peers`                   | partition and eclipse telemetry                         |

Raw bytes are fetched **once**. `getrawmempool` returns ids and bytes cost a
round trip, so a poller that lists a txid the view already holds reports it
without fetching. Paying that round trip per endpoint per transaction would be N
times the work for the same bytes.

## 4. Quorum

The threshold is `⌊N/2⌋ + 1` over the **configured** set, not the responding
set. Majority-of-responding is trivially subvertible: DoS three of five
validators and the remaining two become a "quorum".

An endpoint's **vote is its chain**: its tip and every ancestor in its window.
The **tip** is the highest block that at least threshold endpoints' chains hold
*by hash*, never the maximum height, or one node claiming height 999,999 would
move it. Two blocks at one height cannot both reach a majority, so that block is
unique, and `QuorumTip::agreed_by` is every voter holding it, whether as its own
tip or as an ancestor of it.

Voting on the exact tip instead would split the vote every time a block
propagates: with two validators one block apart, neither tip has a majority even
though both hold the parent. Counting ancestors makes the quorum tip the highest
block a majority can vouch for, so it never drops out mid-propagation.

It also means the tip can **retreat**. If the validator that was ahead stops
voting and the rest lag, the highest block a majority holds is an ancestor of
the old tip, and the view reports that. Block sync sees a retreat inside its
non-final window as a reorg (reset, then replay), which is the honest reading of
"a majority no longer vouches for those blocks". A retreat below the window
cannot be a legal fork (on mainnet and testnet the window is at least the
consensus reorg bound), so the producer waits it out instead of halting, unless
the tip contradicts a block an index committed, which stops it as a divergence.

Chainwork plays no part. The heaviest-chain rule is only sound once proof of
work is verified (the Equihash solution, hash below target, the difficulty
adjustment), which is consensus code Zaino does not hold
([boundaries.md](./boundaries.md)); without those checks a header's work is a
claim, and one lying validator would win. Over an operator-curated set, agreement
by hash is the rule that needs no consensus code.

An endpoint votes while it is `Live`, and also while it is `CatchingUp`: a
validator behind the network tip reports its mempool as inactive, so its
sightings are retracted, but its chain still counts so block sync can follow it.
An endpoint that is `Pending`, `Degraded`, `Down`, or `Syncing` (the node says it
is not ready) does not vote. Chains are `finalised_depth + 1` blocks deep, the
same span as the sync window, so a split deeper than that has no common block and
the view goes below quorum rather than guess.

**Fail closed.** Below threshold, `tip` is `None`. The mempool RPCs refuse with
gRPC `UNAVAILABLE`, the same rule as a syncing index, and block sync waits for
quorum to return. The refusal reports the largest group of voters that hold one
common block. Mempool membership requires quorum too, with one exception (§5).

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

Each published snapshot carries an **epoch**, bumped whenever the tip *block*
changes (a change in `agreed_by` alone moves the tip watch that fetch routing
reads, but leaves the epoch and every open stream alone), and
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
