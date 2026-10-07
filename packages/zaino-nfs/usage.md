# `zaino-nfs` — usage

The non-finalized state (`docs/design/nfs.md`): one folded node per block above the durable
root, one final stream into the index writers, one served `Snapshot` across every index.

## Wiring: `Nfs`

```rust,ignore
use zaino_nfs::{ChainParams, Nfs, NfsError};

let params = ChainParams { network, activations: PoolActivations::from_validator(&info) };
// the one zaino_traffic::TrafficBalancer (its driver spawned elsewhere)
let mut nfs = Nfs::new(header_sync.subscribe(), balancer.clone(), params, lookahead, depth);

// one per enabled index: its final stream out, its committed view in
// value_balance before compact_block (its fees feed compact-block's fold)
let blocks = nfs.subscribe(IndexKind::ValueBalance, writer.committed(), queue_bytes);
tokio::spawn(writer.run(blocks, fee_sink));           // each writer: zaino_sync::Committer inside

let snapshots = nfs.handle();                         // clone per route / stream
let handed = nfs.subscribe_handed();                  // last block handed to the indexes
tasks.spawn(nfs.run(cancel));                          // Err: Diverged | Fold | ChainGone | WriterGone
```

- `Nfs::new`: the `VerifiedChain` watch, the traffic balancer (one `block(hash, urgency)` per
  wanted height: `Tip` above the final tip, `Bulk` below; each answer checked against the
  verified header, a misanswer `report`ed so the re-ask never reaches its sender; who answers,
  hedges and retries are the balancer's), the chain params, `lookahead` (bodies fetched or
  folding ahead of the next one needed), the header chain's reorg depth (unread: side nodes are
  bounded by what the chain `holds`).
- `subscribe(kind, committed, queue)`: enables `kind`. `committed` is a
  `watch::Receiver<V>` of the index store's committed view, sent after every commit: its tip is
  the index's durable tip, and the view is what snapshots and root folds read. Panics on a kind
  twice, or `CompactBlock` before `ValueBalance`.
- `run(cancel)`: cancel → `Ok`; either way the final stream ends with `Shutdown`.
  `Diverged { index, .. }` = an index's durable block off the final chain (resync), `Fold` = an
  index's fold refused a verified block, `ChainGone` / `WriterGone(index)` = an input dropped.
- Folds run on the compute pool (`zaino_sync::compute`), fetches on tasks (a want dropped = its
  task aborted, the balancer's sends with it); final-stream sends are awaited in order (a full
  queue holds the driver back, never grows memory).

## Writers: the final stream

Every `Step::Apply { height, data: Arc<Final> }` is one final block: every height once,
ascending, never retracted. The writers' loop and commit cadence are
[`zaino-sync`](../zaino-sync/usage.md#committer)'s `Committer`.

| `Final.folds` | Meaning                                         | Writer                              |
| ------------- | ----------------------------------------------- | ----------------------------------- |
| `None`        | below the first folded parent (bulk sync)       | folds it itself (`fold`, `fold_run`) |
| `Some(folds)` | folded once by the NFS (the tip)                | `store.apply(folds.get(kind).clone())` |

- Once one step is folded, every later one is too (until a restart).
- **Lockstep finality**: a node leaves only after every enabled index's durable tip reaches it,
  and the first tip fold waits for every index to hold everything sent. A writer must commit when
  its stream idles, not only once its batch fills.
- **Restart**: indexes resume from the lowest durable tip; an index ahead receives heights it
  holds and skips them (value-balance still re-folds a held height for compact-block's fees:
  insert-only, any later state resolves the same).

## Readers: `Snapshot`

```rust,ignore
let snap = snapshots.snapshot().ok_or_else(syncing)?;  // one atomic load, pinned for the request
let tip = snap.tip();                                   // GetLatestBlock: every read answers <= it
let blocks = snap.views().compact_block().ok_or_else(disabled)?;
let trees = snap.views().tree_state();                  // Option: None = index disabled
let located = snap.views().block_hash().map(|r| r.height_of(&hash));
snapshots.changed().await?;                             // next publish (Err: driver stopped)
```

- One snapshot = one served tip across every index: each view = the index's committed view +
  the tip node's layer (rebased onto that view), so a commit or reorg mid-request moves nothing
  it reads.
- `tip()` = the deepest folded block on the verified best, else the root (the lowest durable tip);
  a reorg moves it to the fork point at once and forward as the new branch folds.
- At the root (bulk sync), an index ahead of the root reads past `tip()`: serve at `tip()`.
- `chain()` = the `VerifiedChain` it was cut from, `params()` = network + pool activations.
- `served()` = the served tip as an `At` (`tip()`, `branch()`, `params()`, `views()`); `tip()`,
  `params()`, `views()` on the snapshot are its shorthands.
- Published again per served-tip move **and** per index commit: `durable()` (each index's
  durable tip) is current as of the publish.
- Feature `testing`: `Snapshot::fixed(chain, tip, params, [(kind, committed view)])` (the root at
  `tip`, no layers), `NfsHandle::unpublished()` (nothing served yet) and `NfsHandle::fixed(..)`
  (that snapshot for good): consumers' tests without a driver. `ChainParams::of(&mock_chain, tip)`:
  the network label + pool activations a validator following that `MockChain` at `tip` reports
  (`PoolActivations::from_validator` over its `blockchain_info`), never hard-coded.

### Any folded block: `at`

```rust,ignore
let at = snap.at(&hash).ok_or_else(not_folded)?;       // a folded node (best or side) or the root
match at.branch() { Branch::Best => {}, Branch::Side { from } => {} }
let trees = at.views().tree_state();                    // index state as of `hash`, no I/O
```

| `hash`                                    | `snap.at(hash)`                                    |
| ----------------------------------------- | -------------------------------------------------- |
| served tip                                | = `served()`                                       |
| best node above the root                  | its views (`Branch::Best`)                         |
| side node                                 | its views (`Branch::Side { from }`, `from` = its best parent) |
| root (lowest durable tip)                 | committed views alone                              |
| final below the root / never folded / unknown | `None`                                         |

- An index durable at or past the block reads its committed view alone (heights `<=` the block).
- `folded(hash)` = a node of this snapshot (the root excluded); side nodes = those the header
  chain still `holds` (forking at or above the final tip, its H4 bound): no bound of the NFS's own.

### The publication watch: `indexed()`

`nfs.indexed()` = every publish as a `watch::Receiver<Option<Arc<Snapshot<V>>>>` (`Published<V>`):
the one input `zaino-snapshot`'s publisher reads. `INDEXES` = every kind the NFS folds, fold
order.

## Observability

`describe_metrics()` registers the NFS's metrics (names = ztest's `zainod` families: a rename
breaks its sync probes):

| Signal | Meaning |
|---|---|
| `zaino_best_tip` | the verified best height the NFS follows |
| `zaino_reorgs_total` (0 from boot) + WARN `Chain reorg detected` (`from`, `to`) | a published tip that left the best chain |
| `zaino_fetch_height`, `zaino_fetch_{blocks,transactions,transparent_inputs,transparent_outputs,sapling_spends,sapling_outputs,orchard_actions,ironwood_actions}_total` | blocks handed to the indexes: folded, or sent unfolded (a reorg's branch or a restart counts again; the height rewinds on a reorg) |
| `subscribe_handed()` | the same last handed height, as a watch (zainod's `/statusz` `fetch_height`) |
| INFO `Chain tip advanced` (`height`, `hash`, `age`, `finalized`) | each published tip that is the verified best |
| INFO `Syncing blocks` (`height`, `target`, `bps`, `eta`) / WARN `Block fetch stalled` | every 30 s while the handed height trails the best |

## Folds: `fold_block`

`fold.rs` is the one place indexes meet, in dependency order: value-balance (its fees) →
compact-block → block-hash → tree-state → transparent-address, each only if enabled. A node's
`Folded` = the final stream's `Folds` + one `Layer` per index (`parent layer.with(own Changes)`).

## The core: `NfsCore<F>`

Pure (no I/O, no clock): `step(input) -> Result<Vec<Output>, Diverged>` and `check()` (N1–N5,
`nfs.md` §9; G8, `global-snapshot.md` §6), crate-internal. The driver feeds it the verified
chain, checked bodies, fold results and durable tips, and carries out fetches (one `Fetch` per
want until its body or its `Abandon`), folds, sends and publishes.

## Tests

- `core/model.rs`: random verified-chain evolutions, bodies answered late, out of order or after
  their abandon, delayed folds and commits, restarts, against naive writers and a
  fold-from-genesis oracle (one fetch out per want); every publish's `at` for each node, the
  root, a block below it and a stranger against a naive answer (G7); `core/fire_drills.rs`
  plants one bug per check and precondition. Lying, slow and silent members = `zaino-traffic`'s
  model.
- `fetch.rs`: `check_block` refuses each misanswer by name, every `MockValidator` `Lie` included.
- `tests.rs`: the driver end to end with all five real folds over `SimFs` stores and mock
  validators behind a real balancer (one of them lying), through bulk, reorgs (longer, same
  height, retreat), finality and a crash restart; every snapshot seen: `at` of every mined block,
  side branches included, = each index folded from genesis along that block's path; the driver's
  refusals.
- `fold.rs`: `fold_block` golden (fees in the compact-block record, disabled indexes absent).

```bash
# heavy run: at least 3 minutes after any change to this crate
end=$((SECONDS + 180)); while [ $SECONDS -lt $end ]; do
  PROPTEST_CASES=1000 cargo test -p zaino-nfs || break
done
```
