# `zaino-chainview` — usage

One view over N operator-configured validators: a quorum tip, a quorum mempool,
a broadcast that fans out to every node, and an action stream that says *which*
nodes have seen each transaction. Design: [`docs/design/chainview.md`](../../docs/design/chainview.md).

No durable state, no index files, no replay. Each endpoint is polled, diffed
against its own previous listing, and folded into one published snapshot; a
restart loses nothing one poll round does not restore.

```rust
use std::sync::Arc;
use zaino_chainview::{ChainView, Endpoint};

# fn wire<S: zaino_chainview::EndpointSource>(source: Arc<S>) -> Result<(), Box<dyn std::error::Error>> {
let (view, pollers) = ChainView::new(vec![Endpoint {
    address: "127.0.0.1:8232".to_owned(),
    source,
}])?;
// each poller: `tokio::spawn(poller.run(cancel.child_token()))`
// `view.broadcast(raw)` relays; `view.subscriber()` = the read handle
let _ = (view, pollers);
# Ok(())
# }
```

## In `zainod`

Membership is `[source]` followed by `[[chainview_peers]]`, so endpoint 0 is the
validator the indexes are built from (its adapter is reused, not re-dialled).

```toml
[source]
jsonrpc_address = "127.0.0.1:8232"

[[chainview_peers]]
jsonrpc_address = "10.0.0.7:8232"
```

The daemon spawns one task per poller, ahead of the fetch loop.
`zaino-grpc` (feature `chainview`) routes `SendTransaction`, `GetMempoolTx` and
`GetMempoolStream` to the view; the `CompactTx` projection for `GetMempoolTx`
comes through its `ProjectCompact` port, which `zainod` implements.

## Membership and quorum

- threshold = `⌊N/2⌋ + 1` over the **configured** set (not the responding
  set); N = 1 is valid with threshold 1
- `ChainView::new` rejects an empty list (`ConfigError::NoEndpoints`) and more
  than `EndpointSet::MAX` = 64 (`ConfigError::TooManyEndpoints`)
- the quorum is over the operator-configured set only. `getpeerinfo` is read
  every 60 s into each endpoint's `ValidatorMetadata::peers`: the peer graph a
  discovery pass traverses to find validators to configure, never a vote

## Structure

```text
EndpointPoller × N     one validator each: poll, diff, report added / removed / tip
      │
ChainViewCore          folds deltas into one ChainViewSnapshot, published via ArcSwap
      │
ChainViewSubscriber    readers pin one snapshot per request
```

Each poller owns its interval, backoff and failure count, so a slow validator
degrades alone. The fold is `O(change)`. Raw bytes are fetched once per txid: a
poller skips `getrawtransaction` when the view already holds it.

## Read handle: `ChainViewSubscriber`

Cheap to clone; cannot drive polling or relay.

- `current()` → `Arc<ChainViewSnapshot>`; pin once per request and ask it
  everything
- `quorum()` → `Quorum` (`configured()`, `threshold()`, `met_by(set)`)
- `subscribe_tip()` → `watch::Receiver<Option<QuorumTip>>`, level-triggered
  (`None` = below quorum): what block sync follows (never misses the latest tip)
- `tail()` → `MempoolTail` for one `GetMempoolStream` client

`ChainViewSnapshot` exposes `mempool()` (below) and `endpoints()`: per-endpoint
`ValidatorMetadata` in configured order (address, own tip, `EndpointState`,
`Agreement` with the quorum tip, last-observed time, latency `Ewma`, failure
count, peers).

## Fail closed

`tip()` is the highest block that at least `threshold` voting endpoints report
with the same **hash** (never the max height). `EndpointState::Live` and
`CatchingUp` vote; `Pending`, `Degraded`, `Down` and `Syncing` do not. Below threshold `tip()` is
`None` and `mempool()` returns `Err(BelowQuorum)` instead of an empty answer;
map it to gRPC `UNAVAILABLE`.

```rust
# use zaino_chainview::ChainViewSubscriber;
# fn serve(view: &ChainViewSubscriber, exclude: Vec<Vec<u8>>) -> Result<(), Box<dyn std::error::Error>> {
let pinned = view.current();
let entries = pinned.mempool()?.excluding(&exclude);
# let _ = entries;
# Ok(())
# }
```

`MempoolView` serves only transactions with `seen_at.count() >= threshold` or
`ours`. `excluding(suffixes)` implements `GetMempoolTx`'s rule: a suffix matches
the txid's protocol-order bytes, and a suffix matching two or more entries
excludes none. Entries carry raw transaction bytes and `fee: Option<Zatoshis>`,
the first fee a validator listed (a fee is a function of the transaction, so
every validator lists the same one); `None` = our own broadcast that no
validator has listed yet. `CompactTx` projection is the serving layer's job.

## Broadcast and `ours`

`ChainView::broadcast(raw)` sends to **every** endpoint:

- any accept → `Ok(txid)`, and the transaction is marked `ours` (servable
  before it reaches quorum, so a wallet sees its own send immediately)
- mixed accept/reject → success (usually a stricter local fee filter)
- unanimous domain rejection → `BroadcastError::Rejected`
- no accept and some endpoint unreachable → `BroadcastError::Unreachable`

An `ours` transaction no endpoint lists is dropped when the quorum tip next
moves.

## `Sighting` and `EndpointSet`

`Sighting::seen_at()` is an `EndpointSet` bitset over `EndpointIndex`, not a
count, so it answers *which* nodes: propagation (`1/5 → 4/5`), which no single
node can report. Nothing streams it yet.

`QuorumTip::agreed_by` is the same bitset. `positions()` yields each member's
position in the configured list (the same order as the fetch pool's sources),
and `EndpointSet::at(positions)` builds one from positions.

## `GetMempoolStream`: snapshot, then tail, ending on a block

```rust
# use zaino_chainview::ChainViewSubscriber;
# async fn stream(view: &ChainViewSubscriber) -> Result<(), zaino_chainview::BelowQuorum> {
let mut tail = view.tail()?; // below quorum: refuse the stream (UNAVAILABLE)
for entry in tail.snapshot().entries() {
    // send RawTransaction { data: entry.raw, height: 0 }
}
while let Some(entry) = tail.next().await {
    // same, one per arrival
}
// `None` = quorum tip moved or quorum lost: end the stream
# Ok(())
# }
```

- `snapshot()` is the whole servable mempool at the anchor. Clients reconnect in
  a loop and learn of in-between arrivals only this way.
- `next()` yields each transaction that crossed into servable after the
  snapshot, once. One the snapshot carried is never repeated, even if it drops
  out and back.
- It ends when the **quorum tip moves** (any change, including `A → B → A`
  between two reads), not when the mempool empties. An empty mempool with no new
  block is a live, silent stream, and a test waiting for it to close must mine a
  block.
- It never un-sends; a mined or evicted transaction reaches the client only as
  the block that closes the stream.
- `anchor()` identifies the snapshot. Tails opened on one published view hold
  the same `Arc`, so a serving layer may render it once and share the result.
- Cost per tail: one wake per arrival or tip move (never per propagation
  change), a cursor into the view's shared arrivals log, and the txids it
  delivered after its snapshot.

A transaction leaves the view only when every endpoint stops listing it. A new
tip does not clear the view.

## Cadence and failure

Fixed (`config.rs`): poll 1 s, peer refresh 60 s, backoff 500 ms → 30 s, 10
consecutive failures.

`EndpointPoller::run(cancel)` logs once (INFO) on its first successful tick.
A validator whose mempool is off below the network tip (zebrad's "mempool is not
active") is `CatchingUp`: its tip still votes (so block sync follows a catching-up
validator), its sightings are retracted, and `run` warns every 60 s with its tip
height and hash until the mempool answers, then logs "Validator caught up".
A transport failure marks the endpoint `Degraded` and retries on the backoff
ladder. The failure ceiling, or a validator answering "mempool unavailable",
ejects the endpoint (`Down`, retracting its sightings and vote) and `run`
returns `EndpointPollError`, which `zainod` treats as fatal (it exits). A
cancelled poller returns `Ok(())`.

## Validator port

`EndpointSource` is blanket-implemented over `zaino-source` queries:
`GetChainTip`, `GetMempoolListing`, `GetRawMempoolTransaction`,
`GetMempoolSourceTip`, `SendRawTransaction`, `GetPeerInfo`.

- `GetChainTip` is the readiness probe only: `NotReady` marks the endpoint
  `Syncing`; its value is discarded
- the vote is `GetMempoolSourceTip`, the tip coherent with the listing
- retry is this crate's own per-endpoint ladder
