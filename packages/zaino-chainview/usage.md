# `zaino-chainview` — usage

One view over N operator-configured trusted validators: a tip proven by proof of
work, the validators that hold it, a mempool any of them verified, randomized
submission that watches its own transaction spread, and a stream that says
*which* validators have seen each transaction. Design:
[`docs/design/chainview.md`](../../docs/design/chainview.md).

The view itself keeps no durable state. Every request to the validators goes
through one `zaino_traffic::TrafficBalancer`: each member's poll (the balancer's)
is diffed against its own previous listing and folded into one published
snapshot; a restart loses nothing one poll round does not restore. The one file
behind it is the header chain (`zaino-header-chain`), which `HeaderSync` owns.

```rust
use zaino_chainview::{ChainView, SubmitPolicy};
use zaino_header_chain::HeaderChain;
use zaino_primitives::types::ReorgDepth;
use zaino_traffic::TrafficBalancer;

# fn wire<S: zaino_source::ChainDataSource>(balancer: TrafficBalancer<S>, chain: HeaderChain) -> Result<(), Box<dyn std::error::Error>> {
// addresses[i] = the balancer's trusted member i (logs, status); depth = the header chain's
// finality depth (zainod passes `sync.finalised_depth`)
let addresses = vec!["127.0.0.1:8232".to_owned()];
let view = ChainView::new(addresses, balancer, ReorgDepth::CONSENSUS)?;
let view = view.with_submit_policy(SubmitPolicy::default());
// the tip comes only from header sync, the polls from the fold (the balancer's driver runs
// beside them)
let headers = view.header_sync(chain);
let fold = view.observation_fold();
// each: `tokio::spawn(x.run(cancel.child_token()))`
// `view.submit(raw)` submits; `view.subscriber()` = the read handle
let _ = (view, headers, fold);
# Ok(())
# }
```

## In `zainod`

Membership is `[[trusted_validators]]`, in configured order, all equal for the
view (`priority` only orders who the balancer asks first). One balancer over one
adapter per validator is shared by the view, the NFS and serving.

```toml
[[trusted_validators]]
jsonrpc_address = "127.0.0.1:8232"

[[trusted_validators]]
jsonrpc_address = "10.0.0.7:8232"
```

The daemon spawns the balancer's driver first (the upgrade schedule is a poll's),
then header sync, the observation fold, each push stream (when `indexer_address`
is set: its callbacks → `TrafficBalancer::pushed`) and the peer watch (when
`[p2p]` is on). `zaino-grpc`'s `Routes.submit` holds the view for `SendTransaction`;
`GetMempoolTx`, `GetMempoolStream` and `GetLightdInfo` read its publishes through
`zaino-snapshot` (one global snapshot per request).

## Membership

- `ChainView::new` rejects an empty list (`ConfigError::NoEndpoints`) and more
  than `ValidatorId::MAX` = 64 (`ConfigError::TooManyEndpoints`). N = 1 is valid.
  One address per trusted member of the balancer (asserted).
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
HeaderSync             every validator's headers (headers(Pinned)) → HeaderChain → VerifiedChain
ObservationFold        each member's poll (TrafficBalancer::observe): claim + getblockhash first,
      │                then the listing diff (added / removed, bytes fetched between)
ChainViewCore          folds both into one ChainViewSnapshot (who holds best, agreement), via ArcSwap;
      │                each poll asks getblockhash at its best (TrafficBalancer::poll_best)
      │
ChainViewSubscriber    readers pin one snapshot per request
```

The balancer owns every cadence, retry and failure count, so a slow validator
degrades alone. The fold is `O(change)`. Raw bytes are fetched once per txid: the
fold skips the ones the view already holds and asks `bytes(..)` for the rest, the
lister preferred.
Each `MempoolEntry` carries a `Projection`, the serving layer's encoding of it,
rendered once (`get_or_render`) and shared by every snapshot holding the entry; a
fee arriving for an unpriced entry starts a fresh one.

## The tip: verified, then held

`HeaderSync::run(cancel)` feeds the header chain, which it owns outright (no
lock). On each view change it first vouches every answering validator's claim and
`getblockhash` answer that the chain holds, then fetches one batch (2,000
headers) from the first answering validator whose claim the chain lacks: from
above our best up to its claim, never above `final + depth + 2,000`
(`HeaderChain::ceiling`: the tree stays bounded whatever finality does). Each
batch is verified in two stages, both off the runtime: stage A, each header alone
(version, nBits, solution, Equihash, hash ≤ target) on one blocking task per
core, then the run's links; stage B, in order (attach, difficulty, time, work,
best, bounds), then the run's last header vouched and finality, on the blocking
pool with the chain moved there and back. The header chain's most-work block is
`best`.

`HeaderSync::subscribe()` (take it before `run`) is a
`watch<Option<Arc<VerifiedChain>>>`, republished whenever the best or the final tip
moves (`zaino-header-chain`'s usage: `hash_at`, `header_at`, `locator`). `run` returns `Err(HeaderStoreFailed)` when a final header cannot be
committed: zainod's supervisor ends the process on it.

Each snapshot carries `best()` and `held_by()`:

- `best()` = the header chain's best, never a validator's claim. A validator
  reporting a higher tip moves nothing until its headers arrive and verify.
- `held_by()` = the `Live` validators whose last poll **holds** `best` now
  (`verified-chain.md` §7): its claim = `best`, or its `getblockhash` at the best
  height the poll started under = `best`. A `CatchingUp` validator (mempool off:
  its tip may be stale) never holds; a failed poll (`Degraded`) or `Down` holds
  nothing until it answers again. A race (it moved between the items of one
  batch) costs one wrong poll; the next one re-asks.
- `held_by()` empty → `mempool()` and `validator_info()` are `None`; the global
  snapshot answers `Unavailable::NotHeld` (or `NoChain` with nothing verified).
  Both fail closed (`UNAVAILABLE`): a verified header says the work is real, not
  that the block is valid, and only a validator holding it vouches for the body.

Finality is a different question: was a block ever on a trusted validator's
best chain (**vouched**, permanent: zebra commits only valid blocks)? Each
trusted run vouches its last header (every header of it read by height off that
validator's best chain), as do claims and `getblockhash` answers on our chain,
and final = `min(highest vouched, best − depth)` after every run and vouch. A
first sync from trusted validators therefore finalizes every batch to `best −
depth`; polls never reset that evidence. Work never gates it: peers alone can
never finalize. A block `depth` deep owed finality for 60 s with the final tip
unmoved raises the `finality_paused` alarm; serving continues.

Per validator: a stall (fetch failed, it retreated below its claim, an
undecodable header or one that fails a rule, a header from the future) backs off
that validator alone for 5 s while the next one is asked at once. An undecodable
or rule-failing header is also reported to the balancer (benched: nothing but its
poll reaches it for 60 s, doubling); a header from the future (past the clock +
2 h) is never blamed, nor is an orphan run: retried from the final tip, and
orphaned there too its chain leaves ours below the final tip, so that claim is
not fetched again until it moves.

## Read handle: `ChainViewSubscriber`

Cheap to clone; cannot drive polling or submit.

- `current()` → `Arc<ChainViewSnapshot>`; pin once per request and ask it
  everything
- `subscribe_published()` → `watch::Receiver<()>`, changed on every publish (every fold,
  stored in fold order): `zaino-snapshot`'s publisher reads `current()` on each

`ChainViewSnapshot` exposes `chain()` (the `VerifiedChain` every standing was judged under),
`best()` (verified, held or not), `held_by()`, `arrivals(since)` (transactions servable in it
but not in `since`, `None` = every servable one; an `imbl` diff, so consecutive publishes cost
what changed), `mempool()` (below), `validator_info()` (the first holder's `BlockchainInfo`,
`None` without a holder; `GetLightdInfo` serves it with no validator call), `alarms()`,
`shared_outbound_min()`, `spreads()` and `endpoints()`: per-endpoint `ValidatorMetadata` in
configured order (address, own tip via `tip()`, `zaino_traffic::Health` as of its
last poll, `Agreement` with the verified best block, last-observed time, peers,
release, push-stream state, `blocks_to_end_of_service()`). Latency and failure
counts are the balancer's (`TrafficBalancer::members()`).

`Agreement` comes from its claim and its `getblockhash` answer, as of its last
answered poll: `Agreed` (its claim is the best block), `Ahead` (it claims higher
and its answer is on the verified chain), `Behind` (its claim is on the verified
chain, below the best block), `Diverged` (none of these: a losing branch, an alarm
and never an error), or `Unknown` (nothing verified yet, or its last poll failed).

Tests standing in for header sync (feature `testing`) hand the view a chain with
`ChainView::set_verified(Some(chain.verified(tip)))` (`zaino-header-chain`'s
`testing::HeaderViews` verifies a `MockChain` path's real headers).
`ChainViewSnapshot::fixed(chain, held_by, addresses, ours, unlisted)` builds one view without a
`ChainView`: consumers' tests of what reads it.

Feature `testing` also carries `testing::MockPeers`, the p2p layer as a script
(`ValidatorP2pSource`), beside `zaino_source::testing::MockValidator` for the
validators:

```rust,ignore
let peers = Arc::new(MockPeers::new(live, dead));     // dead entries refuse a push
let view = view.with_peers(peers.clone());
peers.announce(peer, vec![txid]);                    // one `inv` (after peer_watch subscribed)
let entered: Vec<SocketAddr> = peers.pushes();       // entries pushed to, in order
```

`held_by()` is an `EndpointSet` bitset over `ValidatorId`s:
`positions()` yields each member's position in the configured list (the
balancer's order), and `EndpointSet::at(positions)` builds one from positions.

## Mempool

```rust
# use zaino_chainview::ChainViewSubscriber;
# fn serve(view: &ChainViewSubscriber, exclude: Vec<Vec<u8>>) -> Result<(), Box<dyn std::error::Error>> {
let pinned = view.current();
let entries = pinned.mempool().ok_or("no holder: UNAVAILABLE")?.excluding(&exclude);
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
tip does not clear the view. An unlisted `ours` entry is dropped at the next move
of the held best block.

`GetMempoolStream`'s epochs are `zaino-snapshot`'s (keyed by the served tip): it
appends `arrivals(since)` between two consecutive publishes, so the view keeps no
feed of its own.

## Submission

`ChainView::submit(raw)` (§6 of the design):

1. Precheck from the bytes: decodes, expiry height not below the next block
   (ZIP-203; `tx-expiring-soon`), and a v5+ transaction's branch id = the next
   block's. A failure is `SubmitError::Rejected` with no validator contacted.
2. One entry per attempt, drawn uniformly from the balancer's `entries()` (live,
   not benched) not yet tried; each push is `submit(member, raw)`.
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

## Cadence and failure

The balancer's (`zaino-traffic`'s usage, "Polling and observations"): poll 1 s
(15 s while its push stream is up), at least 200 ms apart, metadata every 60 s
(a failed poll leaves it due), 0.5 → 30 s ladder while failing, `Down` after 10
consecutive failures. A push event (`TrafficBalancer::pushed`) polls within
200 ms; `ValidatorMetadata::streaming` reports the stream.

A poll is at most two round trips: the poll batch (claim, listing and the
`getblockhash` answer at the view's best), then one `bytes(..)` for what the view
lacks. The claim and answer are folded before the bytes are fetched. It reads no
headers. A `getblockhash` above the validator's tip, or a failed item, is no
answer; the rest of the poll stands. A failed peer or release
read keeps the last answer and never fails the poll; unanswered bytes are
re-listed and re-fetched next poll.

`ObservationFold::run(cancel)` logs once (INFO) on a validator's first listing.
A validator whose mempool is off below the network tip (zebrad's "mempool is not
active") or absent is `CatchingUp`: it holds no tip (its tip may be stale), its
claim still feeds header sync and vouching, its sightings are retracted, and the fold
warns every 60 s with its tip height and hash until the mempool answers, then logs
"Validator caught up". A failed poll leaves it `Degraded`: its last claim stays
on show, it holds nothing until it answers again, its sightings stay. `Down`
retracts its sightings, claim and answers (never held stale); the balancer keeps
probing every 30 s and its first answer back restores them. A validator going
away never ends `run`; only cancel does. With no holder left the mempool and
validator info fail closed, which is the only consequence.

## Ports

Each trusted validator is a member of a `zaino_traffic::TrafficBalancer` over
`zaino_source::ChainDataSource` (its RPC). The view folds its polls (`observe`)
and asks `bytes(..)` and `submit(..)`; header sync asks `headers(Pinned)`.
Retries, hedges and blame (`report`) are the balancer's.

- an endpoint's claim is the poll's `getblockchaininfo` tip, read in the same
  batch as the listing and the `getblockhash` answer at the view's best height
  (`poll_best`, wired by `ChainView::new`, read as the poll starts); `headers(Pinned)`
  (`getblockheader <h> false`) supplies the raw header bytes of header sync's
  batches, each decoded and hashed once on arrival, never by the source. zebrad
  answers even on an empty state (genesis, mempool inactive = `CatchingUp`), so
  there is no readiness probe
- the whole `BlockchainInfo` is kept per endpoint (see `validator_info()`)

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
`zainod` registers the descriptions through `describe_metrics()`. The
`zaino.chainview.*` state gauges (endpoint state, agreement, heights, peers,
release, holders, alarms, mempool counts) are `zaino-snapshot`'s, set from
`ChainViewSnapshot` at scrape; this crate emits only events:

| Histogram | Labels | Value |
|---|---|---|
| `first_trusted_seconds` | `origin` (`ours`, `network`) | first seen → first trusted listing |
| `all_trusted_seconds` | | first trusted listing → every reader lists it |
| `residence_seconds` | `left` (`block`, `unlisted`), `origin` | first seen → left the view |
| `submission_attempts` | `outcome` | entries one submission pushed to |

`submissions_total` (counter, `outcome`: `spread`, `unconfirmed`, `rejected`,
`unreachable`) counts ended submissions. Bucket edges come with
`METRIC_BUCKETS`.

`alarms()` raises these conditions each fold; `zaino-snapshot`'s publisher logs
one WARN when one rises, one INFO when it clears:

- stale tip: a `Live` endpoint's tip ≥ 24 blocks behind its own clock-based
  estimate
- end of service: an endpoint's release halts within a week (8,064 blocks) of
  its tip
- partition: two live endpoints share no outbound peer
- eclipse: live endpoints reach 1 to 2 distinct outbound peers in total (none at
  all, as on regtest, raises nothing)
- finality paused: a block `depth` deep has been owed finality for 60 s with the
  final tip unmoved, no trusted validator vouching for it (`Alarms::finality_paused`)
