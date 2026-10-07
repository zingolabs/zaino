# `zaino-nfs` — usage

The non-finalized state (`docs/design/nfs.md`): one folded node per block above the durable
root, one final stream into the indexes, one served tip. This crate holds the pure core today;
the async driver, the real fold (`Folded`, `fold_block`) and `Snapshot` arrive with it.

## `NfsCore<F>`: a pure state machine

No I/O, no clock, no randomness: a driver feeds it inputs with the current `Instant` and carries
out the outputs. `F` is one block's folded payload (the real `Folded` in zainod, a toy in tests).

```rust,ignore
use zaino_nfs::{check_block, Answer, Final, Input, NfsCore, Output, SnapshotTip};

// one durable tip per enabled index; `Durable.index` = its position here
let mut core = NfsCore::new(sources.len(), lookahead, depth, durable_tips);

loop {
    let input = next_input().await;          // Chain | Body | Folded | Durable | Tick (1 s)
    for output in core.step(input, Instant::now())? {   // Err(Diverged): resync required
        match output {
            Output::Fetch { from, height, record } => spawn(async move {
                let answer = match sources[from].block(record.hash).await {
                    Ok(block) => check_block(block, height, &record).map_or_else(Answer::Misanswered, Answer::Checked),
                    Err(_) => Answer::Failed,
                };
                Input::Body { from, at: BlockRef { hash: record.hash, height }, answer }
            }),
            Output::Fold { at, parent, block } => spawn_blocking(move || {
                // parent = None: fold on the committed stores at the root
                Input::Folded { at, folded: Arc::new(fold_block(parent, &block)) }
            }),
            Output::Send(Final { block, folded }) => sink.send(Final { block, folded }).await,
            Output::Publish(SnapshotTip { chain, tip, folded }) => publish(chain, tip, folded),
            Output::Misanswered { .. } | Output::Unserved { .. } => { /* status, metrics */ }
        }
    }
}
```

`Send`s must reach the sink in list order; every other output may run in any order.

## What it promises

| Output | Promise |
| --- | --- |
| `Send(Final)` | every height exactly once, ascending, final, never retracted; `folded: None` below the first folded parent (the writer folds), `Some` above it |
| `Fold` | only a block on the verified best, only once its parent is folded; on the root (`parent: None`) only once every index holds everything sent |
| `Publish` | the deepest folded block on the verified best (else the root): a reorg moves it to the fork point at once, forward as the new branch folds |
| `Fetch` | any source may serve any block; nothing unchecked enters |

- **Lockstep finality**: a node leaves only after every enabled index's durable tip reaches it,
  and the first tip fold waits for every index to hold everything sent. A writer must therefore
  commit when its stream idles, not only once its batch fills.
- **Restart**: build a fresh core from each index's durable tip. A tip on the final chain resumes
  from the lowest one (indexes ahead skip what they hold); a tip off it is `Diverged`; a tip above
  the final tip (a lost header store) holds everything until the header chain covers it.
- **Side branches** stay folded while the header chain can still pick them (fork at or above the
  final tip, at most `4 · depth` side nodes): switching back costs no fetch and no fold.

## Invariants and tests

`check()` asserts N1–N5 (`nfs.md` §9) by name; tests and debug drivers run it after every step.
`core/model.rs` drives the core through random verified-chain evolutions, lying and silent
sources, delayed folds and commits, and restarts, against naive writers and a fold-from-genesis
oracle. `core/fire_drills.rs` plants one bug per check and precondition.

```bash
# heavy run: at least 3 minutes after any change to this crate
end=$((SECONDS + 180)); while [ $SECONDS -lt $end ]; do
  PROPTEST_CASES=1000 cargo test -p zaino-nfs || break
done
```
