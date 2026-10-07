# `zaino-chainview` — usage

One view over N operator-configured trusted validators: a tip proven by proof of
work, the validators that hold it, a mempool any of them verified, randomized
submission that watches its own transaction spread, and a stream that says
*which* validators have seen each transaction. Design:
[`docs/design/chainview.md`](../../docs/design/chainview.md).

The view itself keeps no durable state. Each endpoint is polled, diffed against
its own previous listing, and folded into one published snapshot; a restart loses
nothing one poll round does not restore. The one file behind it is the header
chain (`zaino-header-chain`), which `HeaderSync` owns.

```rust
use std::sync::Arc;
use zaino_chainview::{ChainView, Endpoint, SubmitPolicy};
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::ReorgDepth;

# fn wire<S: zaino_source::ChainDataSource>(source: Arc<S>, chain: HeaderChain) -> Result<(), Box<dyn std::error::Error>> {
// depth = the header chain's finality depth (zainod passes `sync.finalised_depth`)
let (view, pollers) = ChainView::new(
    vec![Endpoint { address: "127.0.0.1:8232".to_owned(), source: Arc::clone(&source) }],
    ReorgDepth::CONSENSUS,
)?;
let view = view.with_submit_policy(SubmitPolicy::default());
// the tip comes only from here: spawn it next to the pollers
let headers = view.header_sync(chain, vec![source]);
// each: `tokio::spawn(x.run(cancel.child_token()))`
// `view.submit(raw)` submits; `view.subscriber()` = the read handle
let _ = (view, pollers, headers);
# Ok(())
# }
```

## In `zainod`

Membership is `[[trusted_validators]]`, in configured order, all equal. One
adapter (connection pool) per validator is shared by the view, the fetch pool and
serving; header sync reads through each one's bulk lane.

```toml
[[trusted_validators]]
jsonrpc_address = "127.0.0.1:8232"

[[trusted_validators]]
jsonrpc_address = "10.0.0.7:8232"
```

The daemon spawns header sync, one task per poller (and its push stream, when
`indexer_address` is set, and the peer watch, when `[p2p]` is on), ahead of the
fetch loop. `zaino-grpc`'s `Routes.chain` holds the view: it answers
`SendTransaction`, `GetMempoolTx`, `GetMempoolStream` and `GetLightdInfo`.

## Membership

- `ChainView::new` rejects an empty list (`ConfigError::NoEndpoints`) and more
  than `EndpointSet::MAX` = 64 (`ConfigError::TooManyEndpoints`). N = 1 is valid.
- No validator outvotes another: there is no vote. A validator contributes
  headers (checked against every consensus rule before they count), answers to
  `getblockhash` that say which verified blocks it holds, a mempool listing and a
  submission entry.
- Every 60 s the poll also reads each endpoint's peers
  (`ValidatorMetadata::peers`: address + direction) and release
  (`ValidatorMetadata::release`: build, user agent, protocol, end-of-service
  height): telemetry, never a gate (see Telemetry below).

## Structure

```text
HeaderSync             every validator's headers → HeaderChain (proof of work) → VerifiedChain
EndpointPoller × N     one validator each: poll, diff, report added / removed / claim + getblockhash
      │
ChainViewCore          folds both into one ChainViewSnapshot (Holders: who holds what), via ArcSwap
      │
ChainViewSubscriber    readers pin one snapshot per request
```

Each poller owns its interval, backoff and failure count, so a slow validator
degrades alone. The fold is `O(change)`. Raw bytes are fetched once per txid: a
poller skips the ones the view already holds and fetches the rest in batches.
Each `MempoolEntry` carries a `Projection`, the serving layer's encoding of it,
rendered once (`get_or_render`) and shared by every snapshot holding the entry; a
fee arriving for an unpriced entry starts a fresh one.

## The tip: verified, then held

`HeaderSync::run(cancel)` feeds the header chain, which it owns outright (no
lock). On each view change, for each answering validator whose claim (its own
tip) is not on our best chain, it fetches from above the highest verified block
that validator holds (else above the final tip) up to its claim, in batches of
2,000 headers. Each batch is verified in two
stages, both off the runtime: stage A, each header alone (version, nBits, solution,
Equihash, hash ≤ target) on one blocking task per core, then the run's links;
stage B, in order (attach, difficulty, time, work, best, bounds) with finality,
on the blocking pool with the chain moved there and back. The header chain's
most-work block is `best`. Every move of the chain reaches the view before
finality asks who holds the boundary, so both read one chain.

`HeaderSync::subscribe()` (take it before `run`) is a
`watch<Option<Arc<VerifiedChain>>>`, republished whenever the best or the final tip
moves (`zaino-header-chain`'s usage: `hash_at`, `header_at`, `locator`). `run` returns `Err(HeaderStoreFailed)` when a final header cannot be
committed: zainod's supervisor ends the process on it.

The view serves from `ChainTip { block, held_by }`:

- `block` = `best`, never a validator's claim. A validator reporting a higher
  tip moves nothing until its headers arrive and verify.
- `held_by` = the validators that **hold** `block`. Holding is a question, not
  a walk (`verified-chain.md` §7): a validator holds `(h, hash)` iff its
  `getblockhash h` = `hash`. Each poll asks it at the final boundary (`depth`
  below the best) and at the best, beside its claim; header sync adds the last
  header of each run it reads off that validator's best chain. One answer on the
  verified chain at `h` means it holds every verified block up to `h` (a hash
  commits to its ancestry). Each poll replaces the last one's answers, and a
  failed poll (`Degraded`) or `Down` forgets them: a validator that reorged away
  holds nothing from its next poll on, never a stale vote. A race (it moved
  between the items of one batch) costs one wrong poll; the next one re-asks.
- `held_by` empty → no tip (`Unserved::NotHeld`); nothing verified yet →
  `Unserved::NoBestTip`. Both fail closed (`UNAVAILABLE`): a verified header
  says the work is real, not that the block is valid, and only a validator
  holding it vouches for the body.

Finality moves only past a block `depth` deep and held by a trusted validator
(the same question), so a reorg the trusted set could still follow never crosses
the final boundary. Work never gates it: peers alone can never finalize, and a
trusted holder already vouches for the chain. While a boundary waits for a
holder, finality pauses and the `finality_paused` alarm rises; serving continues.
A validator serving a header that fails a rule is warned and skipped that round;
a header from the future (past the clock + 2 h) is deferred, retried next round
and never blamed; one that retreats below its claim is read again from its next
claim.

The `Holders` core behind it is pure (answers and the `VerifiedChain` in, holders
and agreement out); its `check()` asserts V1 (each validator's reach = the
highest verified height its answers hold) and V2 (agreement = its claim's
classification), after every fold in debug builds. Tests: a model against a
naive oracle over whole validator chains (agree, disagree, failed items, timeouts,
mid-poll reorgs, validators behind and ahead, runs served by header sync) and a
fire drill per check and precondition.

## Read handle: `ChainViewSubscriber`

Cheap to clone; cannot drive polling or submit.

- `current()` → `Arc<ChainViewSnapshot>`; pin once per request and ask it
  everything
- `subscribe_tip()` → `watch::Receiver<Option<ChainTip>>`, level-triggered
  (`None` = unserved): what block sync follows (never misses the latest tip).
  It changes when the tip block changes and when `held_by` alone changes
  (fetch routing reads it)
- `tail()` → `Result<MempoolTail, Unserved>` for one `GetMempoolStream` client

`ChainViewSnapshot` exposes `tip()`, `best()` (verified, held or not),
`unserved()`, `mempool()` (below), `validator_info()` (the first holder's
`BlockchainInfo`, `Err(Unserved)` without a tip; `GetLightdInfo` serves it with
no validator call), and `endpoints()`: per-endpoint `ValidatorMetadata` in
configured order (address, own tip via `tip()`, `EndpointState`, `Agreement`
with the verified best block, last-observed time, latency `Ewma`, failure count,
peers, release, push-stream state, `blocks_to_end_of_service()`).

`Agreement` comes from its claim and its answers, as of its last answered poll:
`Agreed` (its claim is the best block), `Ahead` (it holds the best block and
claims higher), `Behind` (its claim is on the verified chain, below the best
block), `Diverged` (none of these: a losing branch, an alarm and never an error),
or `Unknown` (nothing verified yet, or no answer since its last failure).

Tests standing in for header sync (feature `testing`) hand the view a chain with
`ChainView::set_verified(Some(VerifiedChain::regtest(&path)))`
(`zaino-header-chain`'s `testing` feature verifies the path's real headers).

`ChainTip::held_by` is an `EndpointSet` bitset: `positions()` yields each
member's position in the configured list (the same order as the fetch pool's
sources), and `EndpointSet::at(positions)` builds one from positions.

## Mempool

```rust
# use zaino_chainview::ChainViewSubscriber;
# fn serve(view: &ChainViewSubscriber, exclude: Vec<Vec<u8>>) -> Result<(), Box<dyn std::error::Error>> {
let pinned = view.current();
let entries = pinned.mempool()?.excluding(&exclude); // Err(Unserved) = UNAVAILABLE
# let _ = entries;
# Ok(())
# }
```

`MempoolView` serves a transaction once **any** trusted validator lists it (each
admits only what it fully validated), or it is `ours`. `excluding(suffixes)`
implements `GetMempoolTx`'s rule: a suffix matches the txid's protocol-order
bytes, and a suffix matching two or more entries excludes none. Entries carry raw
transaction bytes and `fee: Option<Zatoshis>`, the first fee a validator listed
(a fee is a function of the transaction, so every validator lists the same one);
`None` = our own submission that no validator has listed yet.

A transaction leaves the view only when every endpoint stops listing it; a new
tip does not clear the view. An unlisted `ours` entry is dropped at the next tip
move.

## Submission

`ChainView::submit(raw)` (§6 of the design):

1. Precheck from the bytes: decodes, expiry height not below the next block
   (ZIP-203; `tx-expiring-soon`), and a v5+ transaction's branch id = the next
   block's. A failure is `SubmitError::Rejected` with no validator contacted.
2. One entry per attempt, drawn uniformly from the validators not yet tried
   (`Down` ones only when nothing else is left).
3. The wallet is answered at the **first acceptance** (`Ok(txid)`; the
   transaction is `ours` from then, servable at once). A rejection or failure is
   answered only once every attempt is spent: no acceptance and some rejection →
   `SubmitError::Rejected` (the first one), no answer at all →
   `SubmitError::Unreachable`. A validator listing the transaction counts as
   an acceptance even when its push answer was lost.
4. The job outlives the answer. After each acceptance it waits
   `propagation_threshold` for the transaction to show up at a validator that was
   never an entry. If it does, the submission ended `spread`; if no such
   validator can be read (N = 1, or every other one down), or every attempt is
   spent unseen, it ends `unconfirmed`; otherwise it pushes to a fresh random
   entry, up to `max_attempts`.

`SubmitPolicy { propagation_threshold, max_attempts }` defaults to 15 s and 4.
`zainod` sets it from `[submission]`.

## Spread: `peers: x/y, trusted: x/y`

Each held transaction's trusted listings are an `EndpointSet` bitset, not a
count, so the view knows *which* validators list it: propagation, which no single
node can report. `ChainViewSnapshot::spread(txid)` (or `spreads()` for all)
answers, servable or not:

- `peers: Count { seen, of }`: announcers still connected, of the live peers
  (an `inv` is an event: an announcer stays on the sighting, counted while live)
- `trusted: Count { seen, of }`: `of` = `mempool_readers()`, the `Live`
  validators (a catching-up or down one lists nothing, so it is not counted)
- `ours`, `servable`
- `timeline`: `first_seen` (the first `inv`, trusted listing or our submission),
  `first_trusted`,
  `all_trusted` (the first moment every reader listed it at once)

The same milestones feed histograms (Telemetry below). Nothing streams it to
wallets yet.

## `GetMempoolStream`: the mempool at the block, then each arrival, ending on a block

One append-only log per tip block, written once and read by cursors (design:
`src/feed.rs`).

```rust
# use zaino_chainview::ChainViewSubscriber;
# fn frame(_: &[u8]) -> bytes::Bytes { bytes::Bytes::new() }
# async fn stream(view: &ChainViewSubscriber) -> Result<(), zaino_chainview::Unserved> {
let mut tail = view.tail()?; // unserved: refuse the stream (UNAVAILABLE)
let opening = tail.opening_rendered(|entries| frame(&entries[0].raw)); // once per block
while let Some(logged) = tail.next().await {
    let record = logged.rendered(|entry| frame(&entry.raw)); // once per transaction
}
// `None` = the tip block moved or the tip went unserved: end the stream
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
- It ends when the **tip block changes** (including a retreat onto an ancestor),
  not when the mempool empties and not when only `held_by` changes. An empty
  mempool with no new block is a live, silent stream.
- `rendered` / `opening_rendered` cache the serving layer's wire bytes: the first
  caller encodes, every other subscriber shares them by refcount. One serving
  layer = one wire form.
- Cost per tail: an `Arc` and a cursor; per arrival, one read lock and one `Arc`
  clone. Nothing grows with the number of arrivals.

## Cadence and failure

Fixed (`config.rs`): poll 1 s (15 s while streaming), at least 200 ms between
polls, metadata refresh 60 s, backoff 500 ms → 30 s, 10 consecutive failures.

`EndpointPoller::waker()` → `PollWaker`, the poller's early-wake handle (a push
stream holds one): `wake()` polls now (coalesced to one pending wake);
`streaming(up)` switches the cadence and polls at once on either edge. The
endpoint's `ValidatorMetadata::streaming` reports it.

A tick is at most two round trips: the poll batch (claim, listing and the two
`getblockhash` answers), then one bytes batch. It reads no headers. A
`getblockhash` above the validator's tip, or one failed item, is no answer for
that height; the rest of the poll stands. A failed peer or release read keeps
the last answer and never fails the tick.

`EndpointPoller::run(cancel)` logs once (INFO) on its first successful tick.
A validator whose mempool is off below the network tip (zebrad's "mempool is not
active") is `CatchingUp`: its answers still count as holding (so block sync
follows a catching-up validator), its sightings are retracted, and `run` warns
every 60 s with its tip height and hash until the mempool answers, then logs
"Validator caught up". A transport failure marks the endpoint `Degraded` and
retries on the backoff ladder. The failure ceiling, or a validator answering
"mempool unavailable", marks it `Down`: its sightings, claim and answers are
retracted (never held stale), and the poller keeps retrying every 30 s; its
first answer back restores them. A `Degraded` endpoint keeps its last claim on
show but holds nothing until it answers again. A validator going away never ends `run`; only cancel does.
With no holder left the tip and mempool fail closed, which is the only
consequence.

## Ports

Each trusted validator is a `zaino_source::ChainDataSource` (its RPC). The view
asks it `get_poll_reading(metadata, holds)`, `get_raw_mempool_transactions` and
`send_raw_transaction`; header sync asks `get_block_links`.

- an endpoint's claim is the poll's `getblockchaininfo` tip, read in the same
  batch as the listing and the `getblockhash` answers at `holds` (the final
  boundary and the best); `get_block_links` (`getblockheader <h> false`) supplies
  the raw header bytes of header sync's batches, each decoded and hashed once on
  arrival, never by the source. zebrad answers even on an empty state (genesis,
  mempool inactive = `CatchingUp`), so there is no readiness probe
- the whole `BlockchainInfo` is kept per endpoint (see `validator_info()`)
- retry is this crate's own per-endpoint ladder

The p2p network is a `ValidatorP2pSource` (`dyn`, so the view's type is the same
with or without it), given by `ChainView::with_peers`:

| Method | Answer |
|---|---|
| `heard()` | a fresh stream of every `Heard { peer, txids }` (`inv`) from now on |
| `live()` | connected peers (`peers: x/y`'s `y`) |
| `entries(tip)` | submission entry candidates: live, on a protocol accepted at `tip` |
| `push(entry, raw)` | one isolated push; `Ok` = delivered (a peer answers nothing) |

`ChainView::peer_watch()` returns the `PeerWatch` to spawn: it folds
announcements every 250 ms, each stamped on arrival. An announced txid the view
does not hold waits in a bounded overheard set (2,000 per peer, 60 s) and joins
its sighting when a trusted validator lists it or it becomes `ours`. With peers,
a submission's entries are peers first (a fresh netgroup per attempt), then one
trusted validator's `sendrawtransaction` as the verdict.

## Telemetry

Observation only: none of it decides membership or gates serving.
`zainod` registers the descriptions through `describe_metrics()`.

| Gauge (`zaino.chainview.*`) | Labels | Value |
|---|---|---|
| `endpoint_state` | `endpoint`, `state` | 1 on the current `EndpointState` |
| `agreement` | `endpoint`, `agreement` | 1 on the current `Agreement` |
| `tip_height` | `endpoint` | the endpoint's own tip height |
| `stale_blocks` | `endpoint` | `estimatedheight` − tip height |
| `peers` | `endpoint`, `direction` | inbound / outbound connections |
| `release` | `endpoint`, `build`, `user_agent` | 1 on the endpoint's release |
| `push_stream` | `endpoint` | 1 while its indexer push streams are up |
| `end_of_service_height` | `endpoint` | height past which its release halts |
| `best_height` | | the header chain's most-work verified block |
| `finality_paused` | | 1 while the final boundary waits for a trusted holder |
| `tip_holders` | | trusted validators holding the verified tip |
| `shared_outbound_min` | | fewest outbound peers two live endpoints share |
| `mempool_transactions` | `state` | held: `verified`, `ours_unverified` |

| Histogram | Labels | Value |
|---|---|---|
| `first_trusted_seconds` | `origin` (`ours`, `network`) | first seen → first trusted listing |
| `all_trusted_seconds` | | first trusted listing → every reader lists it |
| `residence_seconds` | `left` (`block`, `unlisted`), `origin` | first seen → left the view |
| `submission_attempts` | `outcome` | entries one submission pushed to |

`submissions_total` (counter, `outcome`: `spread`, `unconfirmed`, `rejected`,
`unreachable`) counts ended submissions. Bucket edges come with
`METRIC_BUCKETS`.

One WARN when a condition rises, one INFO when it clears:

- stale tip: a `Live` endpoint's tip ≥ 24 blocks behind its own clock-based
  estimate
- end of service: an endpoint's release halts within a week (8,064 blocks) of
  its tip
- partition: two live endpoints share no outbound peer
- eclipse: live endpoints reach 1 to 2 distinct outbound peers in total (none at
  all, as on regtest, raises nothing)
- finality paused: a boundary block is `depth` deep but no trusted validator
  holds it (`Alarms::finality_paused`)
