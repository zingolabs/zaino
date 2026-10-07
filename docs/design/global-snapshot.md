# One global snapshot: what Zaino serves, in one value

Status: **phase 1 built** (additive, §9; 2026-10-07). Builds on [nfs.md](nfs.md) (served tip, views),
[chainview.md](chainview.md) (mempool, validators), [verified-chain.md](verified-chain.md) (best,
final, side branches). Runs beside [traffic-balancer.md](traffic-balancer.md) (§7: the seam).

**Rule:** every read of chain, index, mempool or validator state, by a route, a probe, a scrape
or a log line, comes from one `Arc<Snapshot>` loaded once. Nothing else answers "what do we serve".

## 1. Today: three tips, four publishers

| Concept | zaino-chainview | zaino-nfs | zainod |
| ----------------- | ------------------------------------------------ | --------------------------------------------- | ------------------------------------------ |
| best | `ChainViewSnapshot::best()` (`Holders` chain) | `Snapshot::chain().best()`, `zaino_best_tip` | `status.best_height` |
| held tip / gate | `ChainTip`, `Unserved`, tip watch | — | `headers_syncing`, `tip_not_held` |
| served tip | — | `Snapshot::tip()` (N4), `log_served` | `serving.rs` hysteresis → `synced` watch |
| mempool epoch key | best-held block (`settle` → `rotate`) | — | — |
| progress | — | `subscribe_handed`, `report.rs` | `index_report.rs`, `track_index` (2/index) |
| validators/alarms | `telemetry::emit`: gauges + edges per fold | — | `status.rs` re-maps every field |
| publication | `ArcSwap` + 3 watches | `ArcSwapOption` + watch | 1 + 2N watches |

- Three tips, judged against possibly different `VerifiedChain` versions; `/readyz` = two
  sources at two moments; `GetLightdInfo` = two loads (`blockHeight` vs branch id can tear)
- `GetMempoolStream` closes on a best-held move, before the NFS folds that block (a wallet's
  resubscribe-then-`GetLatestBlock` can still see the old tip)
- 2N + 3 tasks and watches only mirror state into gauges and log lines

## 2. The snapshot

```rust
// zaino-snapshot/src/lib.rs
pub struct Snapshot<V> {
    seq: u64,
    tips: Tips,
    indexed: Option<Arc<Indexed<V>>>,   // zaino-nfs; None = nothing folded or committed yet
    view: Arc<ChainViewSnapshot>,       // chain, holders, mempool, validator facts, alarms
    feed: Feed,                         // GetMempoolStream epoch keyed by tips.served
}

/// - `held_by` = trusted validators holding `best` (V1)
/// - `synced`: opens at served == best, closes off best or > depth behind (today's `at_tip`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tips {
    pub best: Option<BlockRef>,
    pub final_tip: Option<BlockRef>,
    pub served: Option<BlockRef>,
    pub held_by: EndpointSet,
    pub synced: bool,
}

impl<V: View> Snapshot<V> {
    pub fn seq(&self) -> u64;
    pub fn tips(&self) -> Tips;
    pub fn chain(&self) -> Option<&Arc<VerifiedChain>>;
    pub fn served(&self) -> Result<&At<V>, Unavailable>;             // default: best chain
    pub fn at(&self, hash: &BlockHash) -> Option<At<V>>;             // §3
    pub fn forks(&self) -> Vec<ForkView>;                            // §3
    pub fn mempool(&self) -> Result<MempoolView<'_>, Unavailable>;   // gate: held_by != ∅
    pub fn mempool_stream(&self) -> Result<MempoolTail, Unavailable>;
    pub fn lightd(&self) -> Result<(&BlockchainInfo, Height), Unavailable>;  // holder's + served
    pub fn view(&self) -> &ChainViewSnapshot;                        // validator facts, alarms
    pub fn indexed(&self) -> Option<&Arc<Indexed<V>>>;               // memo identity (§4)
    pub fn unready(&self) -> impl Iterator<Item = Unready> + '_;
}

pub enum Unavailable { NoChain, NotHeld { height: u32, configured: usize }, NothingServed }
pub enum Unready { HeadersSyncing, TipNotHeld, Syncing }             // today's reason strings
```

The NFS's `Snapshot` becomes `Indexed`, published on a watch the publisher alone reads:

```rust
// zaino-nfs/src/snapshot.rs
pub struct Indexed<V> {
    chain: Arc<VerifiedChain>,                          // served judged under it
    root: Option<BlockRef>,
    served: At<V>,                                      // built + rebased at publish
    durable: PerIndex<V>,
    graph: imbl::HashMap<BlockHash, Arc<Node<Folded>>>, // O(1) clone, source of at()
}
pub struct At<V> { block: BlockRef, branch: Branch, params: ChainParams, views: Views<V> }
pub enum Branch { Best, Side { from: BlockRef } }
impl<V> At<V> { pub fn tip(&self) -> BlockRef; pub fn params(&self) -> ChainParams; pub fn views(&self) -> &Views<V>; }

impl<S: ChainDataSource, V: SequenceRead + MapRead> Nfs<S, V> {
    pub fn indexed(&self) -> watch::Receiver<Option<Arc<Indexed<V>>>>;   // replaces handle()
    pub fn progress(&self) -> NfsProgress;                               // replaces subscribe_handed()
}
```

- Republished on a served-tip move **and** each `Durable` input (commit): durable tips current
- `At` = today's NFS `Snapshot` minus `chain`: route code keeps `tip()`, `params()`, `views()`

### Crates and direction

```text
  zaino-header-chain ◀── zaino-chainview ◀──┐
          ▲                                  ├── zaino-snapshot ◀── zaino-grpc ◀── zainod
          └────────── zaino-nfs ◀────────────┘          ▲                            │
       (index crates, zaino-sync, zaino-source)          └────────────────────────────┘
```

`zaino-snapshot` (new) = `Snapshot`, `Snapshots`, `Publisher`, `Report`; feeders unaware of it.

### Publisher: one task, one atomic store

```rust
impl<V> Publisher<V> {
    pub fn new(indexed: watch::Receiver<Option<Arc<Indexed<V>>>>, view: ChainViewSubscriber,
               depth: ReorgDepth) -> Self;                    // stores seq 0 at once
    pub fn handle(&self) -> Snapshots<V>;
    pub async fn run(self, cancel: CancellationToken) -> Result<(), SnapshotError>;
}
impl<V> Snapshots<V> {
    pub fn load(&self) -> Arc<Snapshot<V>>;                   // never None (seq 0 at new)
    pub async fn changed(&mut self) -> Result<(), RecvError>;
}
/// Pure (no I/O, no clock): this publish's tips; `was_synced` = the last publish's (hysteresis)
fn compose<V>(was_synced: bool, indexed: Option<&Indexed<V>>, view: &ChainViewSnapshot,
              depth: ReorgDepth) -> Tips;
```

```text
 indexed.changed() ─┐                        ┌─▶ served moved? Feed::open(served, view.arrivals(None))
 view published ────┼─▶ compose(prev, both) ─┤                : prev.feed.append(view.arrivals(prev.view))
 cancel ────────────┘                        ├─▶ ArcSwap::store(Snapshot { seq + 1, feed, .. })
                                             ├─▶ prev.feed.seal() if rotated  (after the store: G5)
                                             └─▶ transitions(prev, next)      (state edges, §5)
```

- No tearing: each input = an immutable `Arc`; snapshot = two `Arc`s + `Copy` tips

- Chain = the chain view's (`HeaderSync::publish` = watch, then `apply_headers`, one call);
  `served` judged against it, as `serving.rs` does today

- Cadence: per input change, `watch`-coalesced; every chain-view fold publishes (mempool
  arrivals included: `GetMempoolTx` reads the latest), arrivals appended to the open epoch

- Feed owned here, not by the chain view (built: no `FeedRotor`): arrivals =
  `ChainViewSnapshot::arrivals(since)`, an `imbl` diff of two consecutive publishes, so the chain
  view never learns the served tip and I4 holds with no third input

- Memory per publish: one ~120 B alloc (2 `Arc` clones, `Feed`, `Tips`); `Indexed` per tip
  move / commit = graph clone O(1) + `served` rebase (≤ blocks committed since the fold); an
  `Epoch` per served move = servable entries' `Bytes` refcounts, opening rendered once

- Pinned stream = one `Indexed` (graph version + disk views) + one `ChainViewSnapshot`, nodes
  shared by refcount; graph ≤ best run + 4·depth side nodes

## 3. Forks and `at(hash)`

```rust
// zaino-header-chain: HeaderChain.{nodes, leaves} → imbl (O(1) into each VerifiedChain)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fork { pub from: BlockRef, pub tip: BlockRef, pub cumulative_work: u128 }
impl VerifiedChain {
    pub fn forks(&self) -> Vec<Fork>;                         // one per side leaf, ≤ SIDE_TIPS
    pub fn branch(&self, tip: &BlockHash) -> Vec<BlockRef>;   // from.next ..= tip
    pub fn holds(&self, at: BlockRef) -> bool;                // best, or side above final
}
// zaino-snapshot
pub struct ForkView { pub fork: Fork, pub folded: Option<BlockRef> }  // deepest NFS node on it
```

| `hash` | `snap.at(hash)` |
| ----------------------------------------- | --------------------------------------------------- |
| served tip | = `served()` |
| best node above root | its node's views: index state as of that block |
| side node in the graph | its views, `Branch::Side { from }` |
| root | committed views alone |
| final below root / never folded / unknown | `None` (final history: heights through `served()`) |

- Cost: graph lookup + per-index `Layer::rebase` (≤ blocks committed since the fold), no I/O
- An index durable at or past the block (a best node below another index's commit): its
  committed view alone, reads at heights `≤` the block (a layer rebases only onto its own blocks)
- Sound: root ≤ final ≤ every fork's `from` (durable views on every node's ancestry)
- NFS pruning = keep a node iff above root and `chain.holds(node.at)`: `graph.rs` `held()` +
  `SIDE_NODES_PER_DEPTH` (a copy of H2/H4) deleted
- Out of scope, fetch + fold of never-folded branches: fetch and fold parents are keyed to the
  best path, and folding attacker-chosen branches is unbounded work
- No new RPC now: forks on `/statusz`; side-hash `GetBlock`/`GetTreeState` = decision 4

## 4. Routes

```rust
pub struct Routes<S: ChainDataSource, V> {
    pub snapshots: Snapshots<V>,         // every read
    pub submit: Arc<ChainView<S>>,       // SendTransaction only
    pub validators: TrafficBalancer<S>,  // GetTransaction, address tx bytes
    pub network: NetworkType,
    pub max_address_rows: NonZeroUsize,
}
// Wired::answer, first line; passed down, never re-loaded
let snap = self.routes.snapshots.load();
```

| Method | Reads |
| -------------------------------------- | ----------------------------------------------------------------------- |
| block, tree-state, transparent methods | `snap.served()?` → `&At` (today's signatures over the NFS `Snapshot`) |
| tree-state memos | keyed on `snap.indexed()` identity (the global `Arc` changes per fold) |
| `GetMempoolTx` | `snap.mempool()?` |
| `GetMempoolStream` | `snap.mempool_stream()?`: epoch at `tips.served`, ends at its rotation |
| `GetLightdInfo` | `snap.lightd()?`: one load |
| `SendTransaction` / `GetTransaction` | `ChainView::submit` / `TrafficBalancer` (no snapshot) |

- Stream end ⇒ next load's `GetLatestBlock` ≥ new tip (store before seal: G5); `Unavailable` →
  `UNAVAILABLE`, today's messages
- Behaviour change: closes when the served tip moves (after the fold), not the best; during sync
  at each served move (not ready: no traffic routed)

## 5. Reporting

| Kind | Home |
| ------------------------------------------------------------ | ------------------------------------------------------- |
| state: tips, readiness, durable tips, validator facts, alarms, mempool counts | f(snapshot, `NfsProgress`), on demand |
| events: `zaino_reorgs_total`, `zaino_fetch_*_total`, histograms, `Chain tip advanced`, `Chain reorg detected`, misanswers, submission ends | where they happen (unchanged) |
| live accounting: sink queue bytes, LSM, gRPC connections, TB `MemberTable` | its owner, read at report time |

```rust
// zaino-snapshot/src/report.rs
#[derive(Debug, Serialize, PartialEq)]
pub struct Report {
    seq: u64,
    tips: TipsReport,                   // best, final, served, held_by/configured, synced
    unready: Vec<Unready>,
    handed: Option<u32>,
    indexes: Vec<IndexReport>,          // name, enabled, durable (empty before the NFS publishes)
    validators: Vec<ValidatorReport>,   // view facts ⨝ MemberTable (traffic-balancer §6)
    alarms: AlarmsReport,
    mempool: MempoolCounts,
    forks: Vec<ForkReport>,             // from, tip, work (decimal string), folded
}
// built (phase 1): `handed` = NfsProgress's one field; `members` joins with TB
impl Report { pub fn of<V: View>(snap: &Snapshot<V>, handed: Option<Height>) -> Self; }
pub fn emit_gauges<V: View>(snap: &Snapshot<V>, handed: Option<Height>);
pub(crate) fn transitions<V>(prev: &Snapshot<V>, next: &Snapshot<V>);  // synced (alarms, finality: step 5)
```

| Surface | Reads | When |
| ------------- | ------------------------------------------------------------------------------------- | ---------- |
| `/readyz` | `snap.unready()` + draining + heartbeat (`starting` / `snapshot_*` before boot) | per probe |
| `/statusz` | `Report::of` + zainod `Process { version, uptime, disk, grpc_sent }` (`serde(flatten)`) | per request |
| `/metrics` | `emit_gauges(snap)` then `render()` (admin thread, `spawn_blocking`) | per scrape |
| edge logs | publisher `transitions(prev, next)` | per publish |
| progress logs | one zainod task: `Syncing` (handed, best, bps, eta, per-index durable); disk walk 120 s | 30 s |

Gauge names = ztest's `family` contract (drift panics ztest), kept verbatim; counters,
histograms and `zaino_build_info` unchanged:

| Name | Today | After (at scrape) |
| ------------------------------------- | ---------------------------- | ----------------------------------- |
| `zaino_best_tip` | NFS, per chain input | `tips.best` |
| `zaino_fetch_height` | NFS `hand()` | `NfsProgress::handed` |
| `zaino_index_finalized_height{index}` | zainod task per index | `Indexed::durable` |
| `zaino_index_synced{index}` | zainod task per index | `tips.synced`, per enabled index |
| `zaino_chainview_*` gauges | `telemetry::emit` per fold | `snap.view()` |

**Deleted**

- zainod: `serving.rs`; `index_report.rs` (disk walk → progress task); `metrics::track_index` +
  `publish_*`; `status.rs` `Sources` / `IndexSource` / per-field mapping; the `synced` watch
- zaino-nfs: `NfsHandle`, `Publisher`, `report.rs`, `emit::best`, the `zaino_fetch_height` set,
  `subscribe_handed`, `graph.rs` `held()` + `SIDE_NODES_PER_DEPTH`
- zaino-chainview: `tip.rs` (`ChainTip`; `Unserved` → `Unavailable`), `subscribe_tip` + tip
  watch, `ChainViewSnapshot::{tip, unserved}`, `feed.rs` + the epoch cell + `rotate` /
  `record_arrivals` + the tails watch (the feed lives in `zaino-snapshot`),
  `ChainViewSubscriber::tail`, `telemetry::emit` (gauges + alarm edge logs)
- zaino-grpc: `Routes.{nfs, chain}` reads, `lightd_info`'s second load

## 6. Invariants and tests

| ID | Invariant | Where |
| --- | ------------------------------------------------------------------------------------------ | ------------------------------- |
| G1 | a request or stream reads one `Snapshot` (routes get `&Snapshot`/`&At`, never the handle) | types; route tests |
| G2 | `seq` + 1 per publish; `tips` = f(that publish's inputs only) | `compose::check`; model |
| G3 | `best`, `final_tip`, `held_by` = `snap.chain()`'s; `served` = the NFS's (on it, or a reorg in flight) | `compose::check`; model |
| G4 | `synced` opens only at served == best; stays only on best and ≤ depth behind | `compose::check`; model |
| G5 | epoch key = `tips.served`; rotates iff served moved; open → store → seal | `compose::check`; model tails |
| G6 | `mempool()` Ok ⇔ `held_by` ≠ ∅; each servable tx streamed once per epoch (V3) | `compose::check`; model tails |
| G7 | `at(h)` Some ⇔ h ∈ graph ∪ {root}; `at(h)` = folding genesis..h along h's branch (N6 + side) | NFS model, driver test |
| G8 | every graph node on best or a `forks()` branch; every fork's `from` ≥ final tip | `Graph::check`; header model |
| G9 | `/statusz`, `/readyz`, gauges = f(one snapshot, counters, owners' live tables) | types; golden `Report` test |

| Test | Scenario → assertions |
| ---------------- | ---------------------------------------------------------------------------------------------------------- |
| publisher model | random header publishes, lagging `Indexed` (reorg, retreat, root moves), folds + arrivals → G2–G6; tails see each tx once per epoch, end iff served moved |
| NFS model | oracle extended to `at(h)` for every node, side included (G7); fire drill per new `check()` assertion (G8) |
| header model | `forks` / `branch` / `holds` vs the tree oracle |
| grpc | one `Snapshots::fixed`: `GetLatestBlock`, `GetBlockRange`, `GetTreeState`, `GetLightdInfo.blockHeight` agree |
| zainod | golden `Report` JSON; every ztest `family` name in a render after `emit_gauges`; pipeline reorg: stream ends, then `GetLatestBlock` = new tip |

## 7. Migration (each step green) and the TrafficBalancer seam

| Step | Scope | Beside TB |
| ---- | -------------------------------------------------------------------------------------------------- | ------------------ |
| 1 | header-chain: `imbl` tree, `Fork`, `forks` / `branch` / `holds`, model | yes |
| 2 | nfs: prune via `holds` (G8); `Indexed`, `At`, `at()`; republish per commit; `NfsProgress`; grpc + zainod read `Indexed` directly | yes (no `fetch.rs`) |
| 3 | `zaino-snapshot`: `Snapshot`, `Publisher`, `compose` + model; `Routes.snapshots`; `serving.rs` gone | yes |
| 4 | chain view: own epochs + `feed.rs` + `tail()` gone (feed = `zaino-snapshot`'s); `ChainTip` / `Unserved` / tip watch gone | after TB step 1 |
| 5 | `Report`, gauges at scrape, `transitions`, one progress task, §5 deletions (name test first) | yes |
| 6 | forks on `/statusz`; decision 4 if taken | yes |
| 7 | docs: nfs.md §6–§7, chainview.md §5/§11/§12, `usage.md` (nfs, chainview, header-chain, snapshot), README index | yes |

TB's "snapshot" side ([traffic-balancer.md](traffic-balancer.md) §6) = the chain-view fold that
turns `Observation`s into `ChainViewSnapshot`; this design starts at that fold's output.

- **I1 NFS**: this owns `snapshot.rs`, `graph.rs` pruning, publish sites; TB owns fetch
  scheduling (`Fetcher`, `Output::Fetch`, `Unserved`, `Misanswered`). Shared: the hand-over
  point keeps calling `NfsProgress` + the `zaino_fetch_*` counters
- **I2 chain view**: TB replaces pollers, `EndpointState` (→ `Health`) and latency/failures (→
  `MemberTable`); the fold, `Holders`, mempool and `ChainViewSnapshot` stay. `Report` joins
  `ChainViewSnapshot` facts (agreement, info, release, peers, streaming) with `MemberTable`
  (health, latency, failures, bench). `fold.rs` edits (deleting `rotate`, `record_arrivals`, the
  epoch cell, the tails watch): step 4, after TB step 1
- **I3 grpc**: `Routes` gains `snapshots` / `submit` here; TB retypes `validators`
- **I4 publication**: the publisher's only inputs = `ChainViewSubscriber` (`current()` + publish
  watch) and the `Indexed` watch, whatever feeds them

## 8. Open decisions

| # | Decision | Recommendation |
| --- | ------------------------------------------ | --------------------------------------------------------------------------------------------- |
| 1 | crate: new / in `zaino-nfs` / in grpc | new `zaino-snapshot` (feeders unaware of each other); zainod `snapshot.rs` → `bootstrap.rs` |
| 2 | epoch key: served tip / best-held | served: stream end ⇒ new tip servable |
| 3 | gauges: at scrape / per publish | scrape: no per-publish cost, always fresh, no mirror tasks |
| 4 | side-hash `GetBlock`/`GetTreeState` via `at` | not yet: lightwalletd clients read a by-hash answer as on-chain; `/statusz` forks first |
| 5 | progress inside the snapshot | no: `handed` moves thousands of times a second in bulk; `NfsProgress` atomics |
| 6 | republish `Indexed` per commit | yes: current durable tips, ≤ one per block at the tip |
| 7 | chain: chain view's / NFS's | chain view's (holders + judgement on the latest); `Indexed.chain` kept for `at()` and asserts |
| 8 | `at()` memo | none until measured (rebase bounded by the root's lag) |

Taken: 1–3, 4 (not yet), 6, 7 as recommended (2026-10-07).

## 9. Build status

**Phase 1 (additive, no call site moved)**

- header-chain: `imbl` tree, `Fork`, `VerifiedChain::{forks, branch, holds}`; model vs each side
  leaf's mined ancestry
- nfs: prune by `holds` (G8 in `Graph::check`); `At`, `Branch`, `Snapshot::{at, served, folded, durable}` (the phase-1 name of `Indexed`); republish per commit; `Nfs::indexed()`; `INDEXES`
- chain view: `ChainViewSnapshot::{chain, arrivals}`, `ChainViewSubscriber::subscribe_published`,
  `ChainViewSnapshot::fixed` (testing)
- `zaino-snapshot`: `Snapshot`, `Tips`, `Unavailable`, `Unready`, `ForkView`, the served-keyed
  feed, `Publisher` / `Snapshots` (`compose`, `check`), `Report`, `emit_gauges`, `transitions`

**Phase 2 (call sites; deletions)**

- nfs: `Snapshot` → `Indexed`; `NfsHandle`, `handle()` and the `tip` / `params` / `views`
  shorthands gone; `NfsProgress` replaces `subscribe_handed` + `report.rs`; `emit::best` and the
  `zaino_fetch_height` set gone; `Nfs::new`'s unread `depth` dropped
- chain view: the §5 deletions (`tip.rs`, `feed.rs`, epoch cell, `tail()`, `telemetry::emit`);
  `compose` reads `held_by` from a holders accessor in place of `tip()`
- snapshot: `Snapshots::fixed` (testing); `Report::of` joins `MemberTable`; `emit_gauges` reads
  `NfsProgress`; alarm / finality edges in `transitions`; `zaino_chainview_*` gauges at scrape
- grpc: `Routes.snapshots` + `submit`; one `load()` per request; `served()?` / `mempool()?` /
  `mempool_stream()?` / `lightd()?`; tree-state memos keyed on `indexed()`
- zainod: wire `Publisher`; delete `serving.rs`, `index_report.rs`, `track_index` + `publish_*`,
  `status.rs` `Sources`; `/statusz` / `/readyz` / `/metrics` from one load; `snapshot.rs` →
  `bootstrap.rs`
- docs: nfs.md §6–§7, chainview.md §5/§11/§12
