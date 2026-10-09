# `zaino-nfs` — usage

The tip overlay (`docs/design/nfs.md`): every block above the indexes' durable tips folded in
RAM, one `Indexed` publish across every index (the index half of `zaino-snapshot`'s global
snapshot). Final blocks reach the writers on the final path (`zaino-sync`), never through here.

## Wiring: `Nfs`

```rust,ignore
use zaino_nfs::{ChainParams, Nfs, NfsError};

let params = ChainParams { network, activations: PoolActivations::from_validator(&info) };
let mut nfs = Nfs::new(header_sync.subscribe(), balancer.clone(), params, depth, lookahead);
let mut follower = FinalFollower::new(header_sync.subscribe(), balancer.clone(), lookahead);

// one per enabled index: its handle to both, its final stream out
let value_balance_writer = ValueBalanceIndexWriter::new(value_balance_store);
let value_balance = value_balance_writer.handle();
let blocks = follower.subscribe(IndexKind::ValueBalance, value_balance.tip(), queue_bytes);
nfs.add(IndexKind::ValueBalance, value_balance);

let publisher = zaino_snapshot::Publisher::new(nfs.indexed(), view.subscriber(), depth);
tasks.spawn(nfs.run(cancel));                          // Err: Fold | ChainGone | IndexGone
```

- `Nfs::new`: the `VerifiedChain` watch, the traffic balancer (`zaino_sync::fetch` per wanted
  block, `Urgency::Tip`), the chain params, `depth` (the NFS folds while the lowest durable tip
  is within `2 · depth` of best), `lookahead` (bodies fetched or folding ahead of the next fold).
- `add(kind, handle)`: enables `kind` (panics on a kind twice). The handle's committed view is
  what snapshots and folds read.
- `run(cancel)`: cancel → `Ok`. `Fold` = an index's fold refused a verified block; `ChainGone` /
  `IndexGone(index)` = an input dropped.

## Readers: `Indexed` → `At`

Routes never hold an `Indexed`: they load one `zaino_snapshot::Snapshot` per request and ask it
`served()?` (this crate's `At`).

```rust,ignore
let at = snap.served()?;                                // zaino-snapshot: one load, pinned
let tip = at.tip();                                     // GetLatestBlock: every read answers <= it
let blocks = at.views().compact_block().ok_or_else(disabled)?; // None = disabled, never syncing
let located = at.views().block_hash().map(|reader| reader.height_of(&hash));
```

- One `Indexed` = one served tip across every enabled index: each view = the committed view +
  the tip node's layer, so a commit or reorg mid-request moves nothing it reads.
- `At::answers_through(kind)` = the highest height `kind` answers: its durable tip when past a
  best-branch block (final data), else the block.
- `served().tip()` = the deepest folded block on the verified best, else the root (the lowest
  durable tip); a reorg moves it to the fork point at once. Nothing durable = no publish
  (`None`).
- `at(hash)`: any folded node (best or side) or the root; `None` = below the root, never folded,
  or unknown. `At::branch()` = `Best` or `Side { from }`.
- `durable()` = each enabled index's durable tip, current as of the publish.
- Feature `testing`: `Indexed::fixed(chain, tip, params, [(kind, view)])`;
  `ChainParams::of(&mock_chain, tip)`.

## Observability

| Signal | Meaning |
|---|---|
| `zaino_reorgs_total` (0 from boot) + WARN `Chain reorg detected` (`from`, `to`) | a published tip that left the best chain |
| INFO `Chain tip advanced` (`height`, `hash`, `age`, `finalized`) | each published tip that is the verified best |

Fetch counters and `zaino_fetch_height` come from the final path (`zaino-sync`).

## Tests

- `core/model.rs`: random chain evolutions, late and stale bodies and folds, writers committing
  after random delays, restarts with a wiped index, against a fold-from-genesis oracle.
- `core/fire_drills.rs`: one planted bug per `check()` assertion and precondition.
- `tests.rs`: `FinalFollower` + the five real writers + the NFS, end to end.

```bash
# heavy run: at least 3 minutes after any change to this crate
end=$((SECONDS + 180)); while [ $SECONDS -lt $end ]; do
  PROPTEST_CASES=1000 cargo test -p zaino-nfs || break
done
```
