# zaino-chainview: one view over many validators

A wallet talking to one node cannot tell "my transaction is stuck" from "my
transaction has propagated", nor "I am behind" from "the chain forked".
`zaino-chainview` answers over N configured validators, and every answer carries
*how many nodes agree*: the thing a client cannot compute for itself.

Membership is `[source]` plus `chainview_peers`; an empty peer list is a
one-validator deployment whose quorum is trivially met. The view serves the
mempool RPCs and `SendTransaction`. Block fetch, `GetTransaction` and
`GetLightdInfo` read `[source]` alone. Scope is JSON-RPC only (§6).
Operating detail is in the crate's [usage guide](../../packages/zaino-chainview/usage.md).

## 1. Membership is configured, never discovered

The endpoint set comes from the operator: an explicit list, or service discovery
inside a trust domain they control. It is never derived from the chain.

**A quorum over a discovered set is not a quorum.** Quorum security rests on the
set being curated: an adversary who can inject endpoints outvotes the honest
members. The Zcash p2p network is not Sybil-resistant (proof-of-work protects
the chain, not a peer table), so `getpeerinfo` is the wrong source for
membership.

It is a good source of *telemetry*. Cross-referencing the configured
validators' peer sets says whether they are partitioned (no peers in common) or
plausibly eclipsed (all sharing one small set). That never decides membership,
so it costs nothing in trust.

## 2. Two layers

```
  EndpointPoller × N          one validator each: poll, diff, publish a delta
        │
        ▼
  ChainView                   folds deltas into one snapshot, published via ArcSwap
        │
        ▼
  ChainViewSnapshot           what every reader pins, once per request
```

Each poller owns its interval, backoff and failure count, so a slow or dead
validator degrades alone. It reports **deltas** (added, removed, tip moved), not
whole mempools, so the fold costs `O(change)` rather than
`O(endpoints × mempool)` per tick.

## 3. The snapshot

```rust
pub struct ChainViewSnapshot {
    tip: Option<QuorumTip>,                        // None below quorum
    mempool: OrdMap<TransactionId, Sighting>,
    endpoints: Vector<ValidatorMetadata>,
    quorum: Quorum,
}

pub struct Sighting {
    seen_at: EndpointSet,                          // bitset over `endpoints`
    first_seen: Instant,
    ours: bool,                                    // relayed by us
    raw: Bytes,
}
```

`seen_at` is a **bitset, not a count**: same size, and it answers "which nodes",
which is what makes eclipse and partition analysis possible.

`ValidatorMetadata` carries what a decision needs:

| Field                | Decision it enables                  |
| -------------------- | ------------------------------------ |
| `tip`, `observed_at` | quorum tip, staleness                |
| `latency` (EWMA)     | routing                              |
| `failures`, `state`  | eject and back off                   |
| `agreement`          | is this node trustworthy *right now* |
| `peers`              | partition / eclipse telemetry        |

Raw bytes are fetched **once**, by the first endpoint to report the txid.
`getrawmempool` gives ids; bytes cost a round trip, and paying it per endpoint
per transaction would be N× waste.

## 4. Quorum

`threshold = ⌊N/2⌋ + 1` over the **configured** set, not the responding set.
Majority-of-responding is trivially subvertible: DoS three of five and the
remaining two become a "quorum".

**Fail closed.** Below threshold, `tip` is `None` and the mempool RPCs refuse
with gRPC `UNAVAILABLE`, the same rule as a syncing index.

**Tip** is the highest block ≥threshold endpoints agree on *by hash*, never the
maximum height (or one node claiming height 999,999 moves it).

**Mempool membership** requires quorum too, with one exception (§5).

## 5. Downstream

### Broadcast (`SendTransaction`)

Fan out to **every** endpoint: N entry points propagate faster than one, and one
dead node cannot block a send.

Succeeds if **any** endpoint accepts. A node rejecting what another accepted
usually has a stricter local policy (a fee filter), not an invalid transaction.
Only a unanimous domain rejection is reported, and then it is the real one.

The transaction is marked `ours` at this moment.

### Mempool RPCs

`GetMempoolTx` and `GetMempoolStream` serve a transaction when
`seen_at.count() >= threshold` **or** `ours`. Without the `ours` exception a
wallet's own transaction would be hidden for the seconds it takes to propagate,
the one case wallets care most about. `SendTransaction` is the one place that
can know.

`GetMempoolStream` is snapshot-then-tail and closes on a mined block, matching
lightwalletd. An empty mempool with no block being mined is a live, silent
stream, not a hang.

### Tails at scale

Every connected pepper-sync wallet holds one stream and reopens it on every
block, so a tail's cost is multiplied by the number of wallets:

- Each published snapshot carries an **epoch** (bumped on every tip change) and
  the epoch's **arrivals**: the txids that crossed into servable, in order.
- A tail pins its anchor snapshot and keeps a cursor into the arrivals. It
  wakes only when an arrival lands or the epoch moves, never on propagation
  churn. Its only per-subscriber state is what it delivered after the snapshot.
- The serving layer renders a snapshot once per published view, then shares it
  across every tail anchored there.

### Propagation data

`RawTransaction` is `{ data, height }`, with nowhere to put propagation data.
Each `Sighting` records *which* endpoints list it (`seen_at`, an `EndpointSet`).
A wallet watching `1/5 → 4/5` would know its transaction is propagating; one
watching `1/5` hold still would know it is not. Neither is observable from a
single node. No stream exposes this yet: the gRPC surface has no field for it.

## 6. Why not p2p

A p2p connection sees transactions earlier and gives propagation topology
natively, but its transactions are **unvalidated**: an `inv` means "a peer
relayed this", where `getrawmempool` means "this node validated and accepted
it". Serving wire sightings would show a wallet transactions no validator
accepted. Combined with pulling Zcash's network stack into an indexer that holds
no consensus code ([boundaries.md](./boundaries.md)), it is not worth the ~1 s
poll lag it saves.
