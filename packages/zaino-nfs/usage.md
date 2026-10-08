# `zaino-nfs` — usage

The non-finalized state (`docs/design/nfs.md`): one folded node per block above the durable
root, one final stream into the index writers, one `Indexed` publish across every index (the
index half of `zaino-snapshot`'s global snapshot).

## Wiring: `Nfs`

```rust,ignore
use zaino_nfs::{ChainParams, Nfs, NfsError};

let params = ChainParams { network, activations: PoolActivations::from_validator(&info) };
// the one zaino_traffic::TrafficBalancer (its driver spawned elsewhere)
let mut nfs = Nfs::new(header_sync.subscribe(), balancer.clone(), params, lookahead);

// one per enabled index: its final stream out, its committed view in
// value_balance before compact_block (its fees feed compact-block's fold)
let blocks = nfs.subscribe(IndexKind::ValueBalance, writer.committed(), queue_bytes);
tokio::spawn(writer.run(blocks, fee_sink));           // each writer: zaino_sync::Committer inside

let publisher = zaino_snapshot::Publisher::new(nfs.indexed(), view.subscriber(), depth);
let progress = nfs.progress();                        // blocks handed, read at report time
tasks.spawn(nfs.run(cancel));                          // Err: Diverged | Fold | ChainGone | WriterGone
```

- `Nfs::new`: the `VerifiedChain` watch, the traffic balancer (one `block(hash, urgency)` per
  wanted height: `Tip` above the final tip, `Bulk` below; each answer checked against the
  verified header, a misanswer `report`ed so the re-ask never reaches its sender; who answers,
  hedges and retries are the balancer's), the chain params, `lookahead` (bodies fetched or
  folding ahead of the next one needed; final steps sent but not yet delivered). Side nodes are
  bounded by what the chain `holds`: no depth of its own.
- `subscribe(kind, committed, queue)`: enables `kind`. `committed` is a
  `watch::Receiver<V>` of the index store's committed view, sent after every commit: its tip is
  the index's durable tip, the view is what snapshots and root folds read, and its schema
  (`View::schema`) shapes the index's layers. Panics on a kind twice, or `CompactBlock` before
  `ValueBalance`.
- `run(cancel)`: cancel → `Ok`; either way the final stream ends with `Shutdown`.
  `Diverged { index, .. }` = an index's durable block off the final chain (resync), `Fold` = an
  index's fold refused a verified block, `ChainGone` / `WriterGone(index)` = an input dropped.
- Folds run on the compute pool (`zaino_sync::compute`), fetches on tasks (a want dropped = its
  task aborted, the balancer's sends with it); final steps are delivered in order beside the
  loop: a full queue pauses the stream (at most `lookahead` steps undelivered, memory bounded),
  never the folds and publishes at the tip.

## Writers: the final stream

Every `Step::Apply { height, data: Arc<Final> }` is one final block: every height once,
ascending, never retracted. The writers' loop and commit cadence are
[`zaino-sync`](../zaino-sync/usage.md#committer)'s `Committer`.

| `Final.folds`             | Meaning                                    | Writer                              |
| ------------------------- | ------------------------------------------ | ----------------------------------- |
| `None`                    | below the first folded parent (bulk sync)  | folds it into `store.changes(block)` (`Run::apply` / `apply_batch`) |
| `Some(folds)`, its kind   | folded once by the NFS (the tip)           | `store.apply(folds.get(kind).clone())` |
| `Some(folds)`, no `kind`  | folded while this index lagged (not joined) | folds it itself, like `None`        |

- Once one step is folded, every later one is too (until a restart); per index, steps it folds
  itself and steps folded for it may interleave (`zaino_sync::Run` keeps height order).
- **Lockstep finality**: a node leaves only after every joined index's durable tip reaches it,
  and the first tip fold waits for every joined index to hold everything sent. A writer must
  commit when its stream idles, not only once its batch fills.
- **Restart**: the stream resumes from the lowest durable tip of every index; an index ahead
  receives heights it holds and skips them (value-balance still re-folds a held height for
  compact-block's fees: insert-only, any later state resolves the same).

### Late indexes: lagging, then joined

An index enabled after the others synced (or behind them after a crash) never holds the served
tip back:

- Boot: joined = the indexes whose durable tip is the highest (value-balance + compact-block
  count as one: the lower of the two); the rest lag. Root = the lowest durable tip of the joined.
- A lagging index is out of folds and snapshots (`Views::syncing(kind)`: its routes answer
  `UNAVAILABLE`) and catches up alone on the stream, from its own tip; the joined ones keep
  folding and serving the tip (a full lagging queue pauses only the stream).
- Joined indexes behind the final tip at boot (blocks arrived while down) are folded on the root
  while the stream sits below it, if within one non-final window (best − final) of the final
  tip; farther behind, they catch up on the stream.
- Its durable tip reaches the root → it joins: the nodes above the root are refolded from their
  own blocks with it (served tip unmoved, republished as each widens), and its views appear in
  snapshots once the served node folds it.

## Readers: `Indexed` → `At`

Routes never hold an `Indexed`: they load one `zaino_snapshot::Snapshot` per request and ask it
`served()?` (this crate's `At`). What an `At` answers:

```rust,ignore
let at = snap.served()?;                                // zaino-snapshot: one load, pinned
let tip = at.tip();                                     // GetLatestBlock: every read answers <= it
let blocks = at.views().compact_block().ok_or_else(disabled)?;
let trees = at.views().tree_state();                    // Option: None = disabled or syncing
let syncing = at.views().syncing(IndexKind::TreeState); // enabled, not served yet: retry
let located = at.views().block_hash().map(|r| r.height_of(&hash));
```

- One `Indexed` = one served tip across every served index: each view = the index's committed
  view + the tip node's layer (rebased onto that view), so a commit or reorg mid-request moves
  nothing it reads.
- `served().tip()` = the deepest folded block on the verified best, else the root (the lowest
  durable tip of the joined indexes); a reorg moves it to the fork point at once and forward as
  the new branch folds.
- At the root (bulk sync), an index ahead of the root reads past the tip: serve at the tip.
- `chain()` = the `VerifiedChain` it was judged under; `At::params()` = network + pool
  activations; `At::branch()` = `Best` or `Side { from }`.
- Published again per served-tip move, per index commit and per join or refold widening the
  served node: `durable()` (each enabled index's durable tip, lagging ones included) is current
  as of the publish.
- Feature `testing`: `Indexed::fixed(chain, tip, params, [(kind, committed view)])` (the root at
  `tip`, no layers; `zaino_snapshot::Snapshots::fixed` pairs it with a chain view) and
  `NfsProgress::fixed(handed)`: consumers' tests without a driver.
  `ChainParams::of(&mock_chain, tip)`: the network label + pool activations a validator following
  that `MockChain` at `tip` reports (`PoolActivations::from_validator` over its
  `blockchain_info`), never hard-coded.

### Any folded block: `at`

```rust,ignore
let at = indexed.at(&hash).ok_or_else(not_folded)?;    // a folded node (best or side) or the root
match at.branch() { Branch::Best => {}, Branch::Side { from } => {} }
let trees = at.views().tree_state();                    // index state as of `hash`, no I/O
```

| `hash`                                    | `indexed.at(hash)`                                 |
| ----------------------------------------- | -------------------------------------------------- |
| served tip                                | = `served()`                                       |
| best node above the root                  | its views (`Branch::Best`)                         |
| side node                                 | its views (`Branch::Side { from }`, `from` = its best parent) |
| root (lowest joined durable tip)          | committed views alone (joined indexes)             |
| final below the root / never folded / unknown | `None`                                         |

- An index durable at or past the block reads its committed view alone (heights `<=` the block).
- An index the block's fold lacks (folded before it joined, not refolded yet) is absent there.
- `folded(hash)` = a node of this publish (the root excluded); side nodes = those the header
  chain still `holds` (forking at or above the final tip, its H4 bound): no bound of the NFS's own.

### The publication watch: `indexed()`

`nfs.indexed()` = every publish as a `watch::Receiver<Option<Arc<Indexed<V>>>>` (`Published<V>`):
the one input `zaino-snapshot`'s publisher reads. `INDEXES` = every kind the NFS folds, fold
order.

## Observability

`describe_metrics()` registers the NFS's event metrics (names = ztest's `zainod` families: a
rename breaks its sync probes). State gauges (`zaino_best_tip`, `zaino_fetch_height`, per-index
durable + synced) and the sync progress lines are read from the global snapshot + `NfsProgress`
at report time (`zaino-snapshot`, zainod's progress task).

| Signal | Meaning |
|---|---|
| `zaino_reorgs_total` (0 from boot) + WARN `Chain reorg detected` (`from`, `to`) | a published tip that left the best chain |
| `zaino_fetch_{blocks,transactions,transparent_inputs,transparent_outputs,sapling_spends,sapling_outputs,orchard_actions,ironwood_actions}_total` | blocks handed to the indexes: folded, or sent unfolded (a reorg's branch or a restart counts again) |
| `progress()` → `NfsProgress` | `handed()` = the last handed height (rewinds on a reorg), `blocks()` = blocks handed since boot; atomics, cloned cheaply, sampled by readers |
| INFO `Chain tip advanced` (`height`, `hash`, `age`, `finalized`) | each published tip that is the verified best |

## Folds: `fold_block`

`fold.rs` is the one place indexes meet, in dependency order: value-balance (its fees) →
compact-block → block-hash → tree-state → transparent-address, each only if served by the
parent (enabled and joined). Each index
folds into the delta its parent layer opens (`parent.layer(kind).changes(block.at())`); a node's
`Folded` = the final stream's `Folds` (those deltas) + one `Layer` per index
(`parent layer.with(delta)`). Readers carry no network: each schema is its store's, read off the
committed view.

## The core: `NfsCore<F>`

Pure (no I/O, no clock): `step(input) -> Result<Vec<Output>, Diverged>` and `check()` (N1–N5,
J1–J4, `nfs.md` §9; G8, `global-snapshot.md` §6), crate-internal. The driver feeds it the
verified chain, checked bodies, fold results, durable tips and deliveries, and carries out
fetches (one `Fetch` per want until its body or its `Abandon`), folds, sends and publishes.

## Tests

- `core/model.rs`: random verified-chain evolutions, bodies answered late, out of order or after
  their abandon, delayed folds, commits and deliveries, restarts (an index wiped at random: it
  lags, then joins; value-balance + compact-block paired or not), against naive writers and a
  per-index fold-from-genesis oracle (one fetch out per want); every publish's `at` for each
  node, the root, a block below it and a stranger against a naive answer (G7);
  `core/fire_drills.rs` plants one bug per check and precondition. Lying, slow and silent
  members = `zaino-traffic`'s model.
- `fetch.rs`: `check_block` refuses each misanswer by name, every `MockValidator` `Lie` included.
- `tests.rs`: the driver end to end with all five real folds over `SimFs` stores and mock
  validators behind a real balancer (one of them lying), through bulk, reorgs (longer, same
  height, retreat), finality and a crash restart; every publish seen: `at` of every mined block,
  side branches included, = each index folded from genesis along that block's path; tree-state
  enabled late with its queue stalled: the four others serve each new tip, it joins once at
  their root; the driver's refusals.
- `fold.rs`: `fold_block` golden (fees in the compact-block record, disabled indexes absent).

```bash
# heavy run: at least 3 minutes after any change to this crate
end=$((SECONDS + 180)); while [ $SECONDS -lt $end ]; do
  PROPTEST_CASES=1000 cargo test -p zaino-nfs || break
done
```
