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
use zaino_primitives::types::ReorgDepth;

# fn wire<S: zaino_chainview::EndpointSource>(source: Arc<S>) -> Result<(), Box<dyn std::error::Error>> {
// depth = each endpoint's ancestry window (zainod passes `fetch.finalised_depth`)
let (view, pollers) = ChainView::new(
    vec![Endpoint { address: "127.0.0.1:8232".to_owned(), source }],
    ReorgDepth::CONSENSUS,
)?;
// each poller: `tokio::spawn(poller.run(cancel.child_token()))`
// `view.broadcast(raw)` relays; `view.subscriber()` = the read handle
let _ = (view, pollers);
# Ok(())
# }
```

## In `zainod`

Membership is `[[trusted_validators]]`, in configured order, all equal. One
adapter (connection pool) per validator is shared by the view, the fetch pool and
serving.

```toml
[[trusted_validators]]
jsonrpc_address = "127.0.0.1:8232"

[[trusted_validators]]
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
  every 60 s into each endpoint's `ValidatorMetadata::peers` (`PeerInfo`:
  address + direction): telemetry, never a vote (see Telemetry below)

## Structure

```text
EndpointPoller × N     one validator each: poll, diff, report added / removed / chain
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
- `quorum()` → `Quorum` (`configured()`, `threshold()`)
- `subscribe_tip()` → `watch::Receiver<Option<QuorumTip>>`, level-triggered
  (`None` = below quorum): what block sync follows (never misses the latest tip).
  It changes when the tip block changes and when `agreed_by` alone changes
  (fetch routing reads it)
- `tail()` → `MempoolTail` for one `GetMempoolStream` client

`ChainViewSnapshot` exposes `mempool()` (below) and `endpoints()`: per-endpoint
`ValidatorMetadata` in configured order (address, own tip via `tip()`,
`EndpointState`, `Agreement` with the quorum tip, last-observed time, latency
`Ewma`, failure count, peers).

`Agreement` is `Agreed` (its tip is the quorum tip), `Ahead` (the quorum tip is
on its chain, below its tip), `Behind` (its tip is on the quorum's chain, below
the quorum tip), `Diverged`, or `Unknown` (no quorum tip, no chain yet, or too
far apart for either window to place).

## Fail closed

Each voting endpoint's vote is its **chain**: its tip plus `depth` ancestors.
The quorum tip is the highest block at least `threshold` of those chains hold by
**hash** (never the max height), and `agreed_by` is every voter holding it, at
its tip or below. So validators one block apart during propagation still agree
on the parent, and when the endpoint that was ahead stops voting the tip
retreats onto the block the rest hold (block sync reads that as a reorg).
`EndpointState::Live` and `CatchingUp` vote; `Pending`, `Degraded`, `Down` and
`Syncing` do not. Below threshold the tip is `None` and `mempool()` returns
`Err(BelowQuorum)` instead of an empty answer; map it to gRPC `UNAVAILABLE`.
`BelowQuorum::agreeing` counts the largest group of voters holding one common
block.

```rust
# use zaino_chainview::ChainViewSubscriber;
# fn serve(view: &ChainViewSubscriber, exclude: Vec<Vec<u8>>) -> Result<(), Box<dyn std::error::Error>> {
let pinned = view.current();
let entries = pinned.mempool()?.excluding(&exclude);
# let _ = entries;
# Ok(())
# }
```

`MempoolView` serves a transaction once **any** validator lists it (each admits
only what it fully validated), or it is `ours`; `seen_at` counts how far it has
spread. `excluding(suffixes)` implements `GetMempoolTx`'s rule: a suffix matches
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

## `EndpointSet`

Each held transaction's sightings are an `EndpointSet` bitset, not a count, so
the view knows *which* nodes list it: propagation (`1/5 → 4/5`), which no single
node can report. Nothing streams it yet.

`QuorumTip::agreed_by` is the same bitset. `positions()` yields each member's
position in the configured list (the same order as the fetch pool's sources),
and `EndpointSet::at(positions)` builds one from positions.

## `GetMempoolStream`: the mempool at the block, then each arrival, ending on a block

One append-only log per tip block, written once and read by cursors (design:
`src/feed.rs`).

```rust
# use zaino_chainview::ChainViewSubscriber;
# fn frame(_: &[u8]) -> bytes::Bytes { bytes::Bytes::new() }
# async fn stream(view: &ChainViewSubscriber) -> Result<(), zaino_chainview::BelowQuorum> {
let mut tail = view.tail()?; // below quorum: refuse the stream (UNAVAILABLE)
let opening = tail.opening_rendered(|entries| frame(&entries[0].raw)); // once per block
while let Some(logged) = tail.next().await {
    let record = logged.rendered(|entry| frame(&entry.raw)); // once per transaction
}
// `None` = quorum tip moved or quorum lost: end the stream
# Ok(())
# }
```

- `opening()` is the servable mempool at the tip block. Clients resubscribe on
  every block and learn of in-between arrivals only this way.
- `next()` yields each transaction that crossed into servable after it, once. One
  the opening carried is never repeated, even if it drops out and back.
- Every tail on a block reads the same log: a late subscriber gets the same
  opening and every arrival since. The stream never un-sends within a block; a
  transaction gone mid-block reaches a client only as the block that ends it.
- It ends when the **quorum tip block changes** (including a retreat onto an
  ancestor), not when the mempool empties and not when only `agreed_by`
  changes. An empty mempool with no new block is a live, silent stream.
- `rendered` / `opening_rendered` cache the serving layer's wire bytes: the first
  caller encodes, every other subscriber shares them by refcount. One serving
  layer = one wire form.
- Cost per tail: an `Arc` and a cursor; per arrival, one read lock and one `Arc`
  clone. Nothing grows with the number of arrivals.

A transaction leaves the view only when every endpoint stops listing it. A new
tip does not clear the view.

## Cadence and failure

Fixed (`config.rs`): poll 1 s, peer refresh 60 s, backoff 500 ms → 30 s, 10
consecutive failures, 16 `getblockheader` calls in flight per endpoint.

Each tick walks the endpoint's chain down from its reported tip until it joins
the chain held from the last tick: one header per new block in steady state,
`depth` headers on the first poll. A validator that reorgs mid-walk keeps last
tick's chain (not a failure). A failed `getpeerinfo` keeps the last peers and
never fails the tick.

`EndpointPoller::run(cancel)` logs once (INFO) on its first successful tick.
A validator whose mempool is off below the network tip (zebrad's "mempool is not
active") is `CatchingUp`: its tip still votes (so block sync follows a catching-up
validator), its sightings are retracted, and `run` warns every 60 s with its tip
height and hash until the mempool answers, then logs "Validator caught up".
A transport failure marks the endpoint `Degraded` and retries on the backoff
ladder. The failure ceiling, or a validator answering "mempool unavailable",
marks it `Down`: its sightings and vote are retracted (never a stale vote), and
the poller keeps retrying every 30 s; its first answer back restores both. A
validator going away never ends `run`; only cancel does. Below quorum the tip
and mempool fail closed, which is the only consequence.

## Validator port

`EndpointSource` is blanket-implemented over `zaino-source` queries:
`GetChainTip`, `GetBlockLink`, `GetMempoolListing`, `GetRawMempoolTransaction`,
`GetBlockchainInfo`, `SendRawTransaction`, `GetPeerInfo`.

- `GetChainTip` is the readiness probe only: `NotReady` marks the endpoint
  `Syncing`; its value is discarded
- the top of the vote is `GetBlockchainInfo`'s tip, coherent with the listing;
  `GetBlockLink` (`getblockheader <h> false`) supplies its ancestry
- the whole `BlockchainInfo` is kept per endpoint: `ChainViewSnapshot::
  validator_info()` = the first quorum-tip agreer's (`Err(BelowQuorum)` below
  quorum), which `GetLightdInfo` serves without a validator call
- retry is this crate's own per-endpoint ladder

## Telemetry

Observation only: none of it votes, decides membership, or gates serving.
`zainod` registers the descriptions through `describe_metrics()`.

| Gauge (`zaino.chainview.*`) | Labels | Value |
|---|---|---|
| `endpoint_state` | `endpoint`, `state` | 1 on the current `EndpointState` |
| `agreement` | `endpoint`, `agreement` | 1 on the current `Agreement` |
| `tip_height` | `endpoint` | the endpoint's own tip height |
| `stale_blocks` | `endpoint` | `estimatedheight` − tip height |
| `peers` | `endpoint`, `direction` | inbound / outbound connections |
| `agreeing` | | largest group of voters holding one common block |
| `shared_outbound_min` | | fewest outbound peers two live endpoints share |

One WARN when a condition rises, one INFO when it clears:

- stale tip: a `Live` endpoint's tip ≥ 24 blocks behind its own clock-based
  estimate
- partition: two live endpoints share no outbound peer
- eclipse: live endpoints reach 1 to 2 distinct outbound peers in total (none at
  all, as on regtest, raises nothing)
