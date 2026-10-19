# `zaino-grpc-snapshot` — usage

One value for everything Zaino serves
([architecture §8](../../docs/architecture.md#8-serving-one-grpcsnapshot)): the chain view's latest,
the NFS's latest and a mempool feed keyed to the served tip, published together as one
`Arc<GrpcSnapshot>`. Every read of chain, index, mempool or validator state, by a route, a probe, a
scrape or a log line, loads one and asks it everything.

## Wiring: `Publisher`

```rust,ignore
use zaino_grpc_snapshot::{Publisher, GrpcSnapshotError};

let publisher = Publisher::new(nfs.indexed(), chain_view.subscriber(), depth); // seq 0 stored now
let grpc_snapshots = publisher.handle();               // clone per route / probe / scrape
tasks.spawn(publisher.run(cancel));                    // Err(IndexedGone): the NFS stopped
```

- Inputs = the NFS's publication watch (`Nfs::indexed()`) and the chain view's
  (`subscribe_published()` + `current()`), nothing else; `depth` = the header chain's reorg depth.
- Each change of either → one publish from the latest of both (`watch`-coalesced: a burst
  publishes once). A publish = two `Arc` clones + `Copy` tips + the feed: never torn.
- The chain is the chain view's (`ChainViewSnapshot::chain()`): `best`, `finalized_tip` and
  `synced` are judged under it; the NFS's own chain stays inside its `at()`.

## Readers: `GrpcSnapshots` → `GrpcSnapshot`

```rust,ignore
let grpc_snapshot = grpc_snapshots.load();             // one atomic load, never None: pin it
let at = grpc_snapshot.served()?;                      // Err(Unavailable): UNAVAILABLE + its message
let blocks = at.views().compact_block();
let mempool = grpc_snapshot.mempool()?;                // gate: a Live holder of best, or a peer quorum
let lightd = grpc_snapshot.lightd()?;                  // GetLightdInfo: one load, no validator
grpc_snapshots.changed().await?;                       // next publish
```

| Method                  | Answer                                                                        |
| ----------------------- | ----------------------------------------------------------------------------- |
| `seq()`                 | +1 per publish                                                                |
| `tips()`                | `Tips { best, finalized_tip, served, synced }`                                |
| `chain()`               | the chain view's `HeaderChainSnapshot`                                        |
| `served()`              | the NFS's served `IndexesAtBlock` (`tip()`, `branch()`, `params()`, `views()`); `NothingServed` before its first publish |
| `at(hash)`              | index state as of any folded block (best or side) or the NFS root; `None` otherwise |
| `mempool()`             | the servable mempool while a `Live` validator holds best or `peers_quorate()`; else `NoChain` / `NotHeld` |
| `mempool_stream()`      | a `MempoolTail` on the epoch at `tips.served` (same gate)                     |
| `lightd()`              | `Lightd { served, best, schedule }`: the served height, the verified best's (the network tip estimate) and the served `IndexesAtBlock`'s upgrade schedule; `NothingServed` before the first publish, never a validator (served degraded too) |
| `chain_view()`          | the chain view's snapshot: validator facts, alarms, spreads                   |
| `indexed()`             | the NFS publish (`Arc` identity = a memo key: new per NFS publish)            |
| `serving_status(now_unix)` | first match: best more than one block past the last vouched (`HeaderChainSnapshot::vouched`) → `Degraded` while synced to a fresh best (its block time within `zaino_chainview::STALE_TIP_BLOCKS` (24) target spacings of `now_unix`: peers carried the tip), else `Down`; `Syncing` (not synced, or a validator `CatchingUp` and none `Live`: zebrad's mempool off after a catch-up); `Serving` |
| `sync_target()`         | best's height, or a catching-up validator's clock estimate when higher        |

- `Unavailable`'s `Display` = the gRPC `UNAVAILABLE` message: `no verified header chain tip yet`,
  `no trusted validator holds the verified tip {h} (of {n} configured)`, `the indexes are syncing:
  nothing served yet`.
- Feature `testing`: `GrpcSnapshots::fixed(indexed, view)` = one snapshot for good, no publisher
  (`changed()` errs, a tail ends after its opening): route tests over `NfsPublication::fixed` +
  `ChainViewSnapshot::fixed`.

- `synced` ([when it opens and closes](../../docs/architecture.md#8-serving-one-grpcsnapshot))
  compares hashes, not heights; `serving_status` = `Syncing` until it opens.

## `GetMempoolStream`: one epoch per served tip

```rust,ignore
let mut tail = grpc_snapshot.mempool_stream()?;
send(tail.opening_rendered(render_all));               // servable at the epoch's opening
while let Some(logged) = tail.next().await {            // each arrival once; None = served moved
    send(logged.rendered(render_one));
}
```

- Epoch key = `tips.served`: a new epoch opens when the served tip moves, is stored in that
  publish, and only then is the old one sealed (so a stream ends only once the new tip is
  served).
- Arrivals = `ChainViewSnapshot::arrivals` between two consecutive publishes (an `imbl` diff),
  appended to the open epoch; an epoch never carries a transaction twice nor un-sends one.
- Never a transaction of the served tip's own chain: the opening and every arrival skip the
  txids of `NfsPublication::served_above_durable()`, collected once per epoch (the chain view's listings
  trail a block by up to a poll, so its mempool still lists what the new tip mined).
- Contract S1–S4 (bound to the tip hash, never mined, once per stream, `height: 0`):
  [architecture §8](../../docs/architecture.md#getmempoolstream).

## Reporting

```rust,ignore
describe_metrics();                                     // once, at boot
emit_gauges(&grpc_snapshots.load(), &nfs_progress);     // per scrape, then render
```

- `notes(&grpc_snapshot)`: one sentence per raised alarm (zainod's `/status` `notes`).
- Gauges, all set at scrape from one load (names = ztest's `zainod` families): `zaino_best_tip`,
  `zaino_fetch_height` (`handed`), `zaino_index_synced{index}`, and the chain view's
  `zaino_chainview_*` state:
  `endpoint_state{endpoint,state}`, `agreement{endpoint,agreement}`, `tip_height{endpoint}`,
  `stale_blocks{endpoint}`, `peers{endpoint,direction}`, `push_stream{endpoint}`,
  `release{endpoint,build,user_agent}`, `end_of_service_height{endpoint}`, `tip_holders`,
  `finality_paused`, `shared_outbound_min`, `mempool_transactions{state}` (`verified`, `quorum`,
  `ours_unverified`).
- Edge logs per publish (`transitions`): INFO `Serving the verified tip` / `Behind the verified
  tip, syncing` when `synced` flips; WARN on a rise, INFO on a clear, for each chain view alarm
  ([architecture §11](../../docs/architecture.md#11-telemetry)).

## Invariants and tests

| ID | Invariant | Where |
| --- | --- | --- |
| G1 | a request or stream reads one `GrpcSnapshot` (routes get `&GrpcSnapshot` / `&IndexesAtBlock`, never the handle) | types; route tests |
| G2 | `seq` + 1 per publish; `tips` = f(that publish's inputs only) | `compose::check`; model |
| G3 | `best`, `finalized_tip` = `chain()`'s; `served` = the NFS's | `compose::check`; model |
| G4 | `synced` opens only at served == best with no validator claiming past it; stays only on best and ≤ depth behind | `compose::check`; model |
| G5 | epoch opened at `tips.served`; rotates iff served moved; open → store → seal | `compose::check`; model |
| G6 | `mempool()` Ok ⇔ a `Live` validator in the view's `held_by()`, or `peers_quorate()`; each servable tx streamed once per epoch | `compose::check`; model |
| G7 | `at(h)` Some ⇔ h folded or the root; = folding genesis..h along h's branch | NFS model |
| G8 | every NFS node on best or a side branch the chain holds (`HeaderChainSnapshot::holds`) | `Graph::check` |
| G9 | `/readyz`, `/status`, gauges = f(one snapshot, counters) | types; gauge golden |
| G10 | an epoch carries no txid of a block in `served_above_durable()` | `compose::check`; model |

`compose::check(prev, next, depth)` is test-only: the model calls it after every publish.

- `model.rs`: random header moves (extend, reorg, finalize), holders, relays / sightings /
  listings / drops, NFS served tips (lagging, on an old best), coalesced publishes and tails
  opened at random, against naive tips and per-epoch transaction sets.
- `tests.rs`: the run loop over real watches (seq 0, coalescing, cancel, a gone NFS); the
  rendered gauges of one snapshot; one fire drill per `check` assertion.

Heavy run required after changes: see [docs/testing.md](../../docs/testing.md#heavy-run).
