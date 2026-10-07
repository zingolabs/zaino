# `zaino-nfs` — usage

The non-finalized state (`docs/design/nfs.md`): one folded node per block above the durable
root, one final stream into the index writers, one served `Snapshot` across every index.

## Wiring: `Nfs`

```rust,ignore
use zaino_nfs::{ChainParams, Nfs, NfsError};

let params = ChainParams { network, activations: PoolActivations::from_validator(&info) };
let sync = sources.iter().map(|source| Arc::new(source.on(Lane::Sync))).collect();
let mut nfs = Nfs::new(header_sync.subscribe(), sync, params, lookahead, depth);

// one per enabled index: its final stream out, its committed view in
// value_balance before compact_block (its fees feed compact-block's fold)
let blocks = nfs.subscribe(IndexKind::ValueBalance, writer.committed(), queue_bytes);
tokio::spawn(writer.run(blocks, fee_sink));           // each writer: zaino_sync::Committer inside

let snapshots = nfs.handle();                         // clone per route / stream
let handed = nfs.subscribe_handed();                  // last block handed to the indexes
tasks.spawn(nfs.run(cancel));                          // Err: Diverged | Fold | ChainGone | WriterGone
```

- `Nfs::new`: the `VerifiedChain` watch, the block sources (any may serve any block: each answer
  checked against the verified header), the chain params, `lookahead` (bodies fetched or folding
  ahead of the next one needed), the header chain's reorg depth.
- `subscribe(kind, committed, queue)`: enables `kind`. `committed` is a
  `watch::Receiver<V>` of the index store's committed view, sent after every commit: its tip is
  the index's durable tip, and the view is what snapshots and root folds read. Panics on a kind
  twice, or `CompactBlock` before `ValueBalance`.
- `run(cancel)`: cancel → `Ok`; either way the final stream ends with `Shutdown`.
  `Diverged { index, .. }` = an index's durable block off the final chain (resync), `Fold` = an
  index's fold refused a verified block, `ChainGone` / `WriterGone(index)` = an input dropped.
- Folds run on the compute pool (`zaino_sync::compute`), fetches on tasks; final-stream sends are
  awaited in order (a full queue holds the driver back, never grows memory).

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
- Feature `testing`: `NfsHandle::unpublished()` (nothing served yet) and
  `NfsHandle::fixed(chain, tip, params, [(kind, committed view)])` (one snapshot for good, no
  layers): consumers' route tests without a driver.

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

Pure (no I/O, time as input): `step(input, now) -> Result<Vec<Output>, Diverged>` and `check()`
(N1–N5, `nfs.md` §9), crate-internal. The driver feeds it the verified chain, checked bodies,
fold results and durable tips, and carries out fetches, folds, sends and publishes.

## Tests

- `core/model.rs`: random verified-chain evolutions, lying and silent sources, delayed folds and
  commits, restarts, against naive writers and a fold-from-genesis oracle; `core/fire_drills.rs`
  plants one bug per check and precondition.
- `tests.rs`: the driver end to end with all five real folds over `SimFs` stores and mock
  validators, through bulk, reorgs (longer, same height, retreat), finality and a crash restart;
  every snapshot seen = each index folded from genesis along best; the driver's refusals.
- `fold.rs`: `fold_block` golden (fees in the compact-block record, disabled indexes absent).

```bash
# heavy run: at least 3 minutes after any change to this crate
end=$((SECONDS + 180)); while [ $SECONDS -lt $end ]; do
  PROPTEST_CASES=1000 cargo test -p zaino-nfs || break
done
```
