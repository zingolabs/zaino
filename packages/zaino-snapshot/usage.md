# `zaino-snapshot` — usage

One value for everything Zaino serves (`docs/design/global-snapshot.md`): the chain view's
latest, the NFS's latest and a mempool feed keyed to the served tip, published together as one
`Arc<Snapshot>`. Every read of chain, index, mempool or validator state, by a route, a probe, a
scrape or a log line, loads one and asks it everything.

## Wiring: `Publisher`

```rust,ignore
use zaino_snapshot::{Publisher, SnapshotError};

let publisher = Publisher::new(nfs.indexed(), chain_view.subscriber(), depth); // seq 0 stored now
let snapshots = publisher.handle();                    // clone per route / probe / scrape
tasks.spawn(publisher.run(cancel));                    // Err(IndexedGone): the NFS stopped
```

- Inputs = the NFS's publication watch (`Nfs::indexed()`) and the chain view's
  (`subscribe_published()` + `current()`), nothing else; `depth` = the header chain's reorg depth.
- Each change of either → one publish from the latest of both (`watch`-coalesced: a burst
  publishes once). A publish = two `Arc` clones + `Copy` tips + the feed: never torn.
- The chain is the chain view's (`ChainViewSnapshot::chain()`): `best`, `final_tip`, `held_by`
  and `synced` are judged under it; the NFS's own chain stays inside its `at()`.

## Readers: `Snapshots` → `Snapshot`

```rust,ignore
let snap = snapshots.load();                           // one atomic load, never None: pin it
let at = snap.served()?;                               // Err(Unavailable): UNAVAILABLE + its message
let blocks = at.views().compact_block();
let mempool = snap.mempool()?;                         // gate: a trusted validator holds best
let (info, height) = snap.lightd()?;                   // GetLightdInfo: one load
snapshots.changed().await?;                            // next publish
```

| Method                  | Answer                                                                        |
| ----------------------- | ----------------------------------------------------------------------------- |
| `seq()`                 | +1 per publish                                                                |
| `tips()`                | `Tips { best, final_tip, served, held_by, synced }`                           |
| `chain()`               | the chain view's `VerifiedChain`                                              |
| `served()`              | the NFS's served `At` (`tip()`, `branch()`, `params()`, `views()`); `NothingServed` before its first publish |
| `at(hash)`              | index state as of any folded block (best or side) or the NFS root; `None` otherwise |
| `forks()`               | each side branch of `chain()` (`ForkView { fork, folded }`, `folded` = its deepest folded block) |
| `mempool()`             | the servable mempool; `NoChain` / `NotHeld` while no trusted validator holds best |
| `mempool_stream()`      | a `MempoolTail` on the epoch at `tips.served` (same gate)                     |
| `lightd()`              | the first holder's `BlockchainInfo` + the served height (`NoChain` / `NotHeld` / `NothingServed`) |
| `view()`                | the chain view's snapshot: validator facts, alarms, spreads                   |
| `indexed()`             | the NFS publish (`Arc` identity = a memo key: new per NFS publish)            |
| `unready()`             | `HeadersSyncing`, `TipNotHeld`, `Syncing`, in that order (`label()` = `/readyz` text) |

- `Unavailable`'s `Display` = the gRPC `UNAVAILABLE` message: `no verified header chain tip yet`,
  `no trusted validator holds the verified tip {h} (of {n} configured)`, `the indexes are syncing:
  nothing served yet`.
- Feature `testing`: `Snapshots::fixed(indexed, view)` = one snapshot for good, no publisher
  (`changed()` errs, a tail ends after its opening): route tests over `Indexed::fixed` +
  `ChainViewSnapshot::fixed`.

- `synced` opens once the served tip **is** the best block (hash, not height) and stays open
  while the served tip is on the best chain and at most `depth` below it.

## `GetMempoolStream`: one epoch per served tip

```rust,ignore
let mut tail = snap.mempool_stream()?;
send(tail.opening_rendered(render_all));               // servable at the epoch's opening
while let Some(logged) = tail.next().await {            // each arrival once; None = served moved
    send(logged.rendered(render_one));
}
```

- Epoch key = `tips.served`: a new epoch opens when the served tip moves, is stored in that
  publish, and only then is the old one sealed. A stream's end ⇒ the next `load()` serves the
  new tip (`GetLatestBlock` ≥ it).
- Arrivals = `ChainViewSnapshot::arrivals` between two consecutive publishes (an `imbl` diff),
  appended to the open epoch; an epoch never carries a transaction twice nor un-sends one.

## Reporting

```rust,ignore
let members = balancer.members().borrow().clone();     // the traffic balancer's MemberTable
let body = Report::of(&snapshots.load(), &nfs_progress, &members, &enabled); // `/statusz`
let stub = indexes(&enabled, &[]);                      // same list, before a snapshot exists
describe_metrics();                                     // once, at boot
emit_gauges(&snapshots.load(), &nfs_progress);          // per scrape, then render
```

- `Report`: seq, tips (heights + hashes, `held_by` / `configured`, `synced`), unready reasons,
  `handed` (`SyncProgress::handed`), every index the NFS folds (`enabled` = the caller's
  configured set, so listed before the NFS's first publish; `durable` = `null` until then), each
  validator's facts (agreement, own height, staleness, push streams, release, peers) joined with
  its `MemberTable` row (`state`, `latency_ms` once measured, `failures`), alarms (incl.
  `finality_paused`), mempool counts, forks (work as a decimal string). zainod flattens it beside
  its process fields.
- Gauges, all set at scrape from one load (names = ztest's `zainod` families): `zaino_best_tip`,
  `zaino_fetch_height` (`handed`), `zaino_index_finalized_height{index}`,
  `zaino_index_synced{index}`, and the chain view's `zaino_chainview_*` state:
  `endpoint_state{endpoint,state}`, `agreement{endpoint,agreement}`, `tip_height{endpoint}`,
  `stale_blocks{endpoint}`, `peers{endpoint,direction}`, `push_stream{endpoint}`,
  `release{endpoint,build,user_agent}`, `end_of_service_height{endpoint}`, `tip_holders`,
  `best_height`, `finality_paused`, `shared_outbound_min`, `mempool_transactions{state}`.
- Edge logs per publish (`transitions`): INFO `Serving the verified tip` / `Behind the verified
  tip, syncing` when `synced` flips; WARN on a rise, INFO on a clear, for each chain view alarm
  (stale validator tip, end of service, partition, eclipse, finality paused).

## Invariants and tests

`compose::check(prev, next, depth)` runs after every publish in debug builds: G2 (seq + 1), G3
(tips = this publish's inputs), G4 (synced hysteresis), G5 (epoch key = served, rotates iff it
moved, old sealed, stored open), G6 (`mempool()` gate = `held_by`).

- `model.rs`: random header moves (extend, reorg, finalize), holders, relays / sightings /
  listings / drops, NFS served tips (lagging, on an old best), coalesced publishes and tails
  opened at random, against naive tips and per-epoch transaction sets.
- `tests.rs`: the run loop over real watches (seq 0, coalescing, cancel, a gone NFS); the
  `/statusz` golden (a `MemberTable` joined) and its rendered gauges; one fire drill per `check`
  assertion. `report.rs`: the end-of-service serialization contract.

```bash
# heavy run: at least 3 minutes after any change to this crate
end=$((SECONDS + 180)); round=0; while [ $SECONDS -lt $end ]; do
  round=$((round + 1)); PROPTEST_CASES=1000 cargo test -p zaino-snapshot || break
done
```
