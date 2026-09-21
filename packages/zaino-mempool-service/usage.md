# Using `zaino-mempool-service`

A task-oriented guide to the mempool **runtime**: how to spawn the tip-agnostic
core, consume its reads and change feed, and (with `tip_aware_mempool`) layer the
tip-aware coherence service and stream the mempool.

This crate implements the ports defined in
[`zaino-mempool`](../zaino-mempool/usage.md) — read that first for the
model and the change-feed consistency contract. For the wired-up production example
see `zaino-state`'s `chain_index` (it owns both services and routes reads).

The snippets below are illustrative sketches, not copy-paste-complete programs.

## Spawn the core

`MempoolService<S>` is generic over `S: MempoolSource` — any validator adapter
answering the four mempool `zaino-source` ports plus `SubscribeBlocks`. Spawn it with a
config and a cancellation token; it starts a background poll task immediately.

```rust
use tokio_util::sync::CancellationToken;
use zaino_mempool::MempoolConfig;
use zaino_mempool_service::MempoolService;

let cancel = CancellationToken::new();
let core = MempoolService::spawn(source /* : impl MempoolSource */, MempoolConfig::default(), cancel.child_token());
let mempool = core.subscriber(); // cheap, cloneable read handle
```

`core.close()` publishes a final `Closing` update and stops the task;
`core.status()` reports health.

### Capacity control

The memory bound lives on the **service**, not the read handle — it is a
capacity-control knob for whoever owns the mempool:

```rust
core.set_max_cost_bytes(64 * 1024 * 1024); // takes effect next poll
let bound = core.max_cost_bytes();         // also readable via mempool.max_cost_bytes()
```

Lowering it does not evict: the set shrinks as transactions are mined, and
additions over the bound are refused meanwhile (snapshot `completeness` becomes
`IncompleteCapacityLimited`). Refused transactions are fetched once, not once per
poll, and are re-admitted automatically once there is room.

## Read the live mempool (tip-agnostic)

`MempoolSubscriber` serves the never-frozen reads:

```rust
let snap = mempool.snapshot();          // Arc<MempoolSnapshot>, lock-free load
let info = mempool.get_mempool_info();  // { size, bytes, usage } for getmempoolinfo
let txids = mempool.get_txids();        // getrawmempool
let entry = mempool.get_transaction(&txid);
let present = mempool.contains_txid(&txid);

// GetMempoolTx exclude filter (bounded; validate before use):
let suffixes = mempool.validate_exclude_suffixes(&client_endian_suffixes)?;
let entries = mempool.get_filtered_entries(&suffixes);
```

Derive wire/compact forms from `entry.serialized_bytes()` / `entry.wire_bytes()`
at your boundary — the entry holds no RPC types.

## Consume the change feed

Prefer the ergonomic stream, which surfaces a lag explicitly (never a silent skip):

```rust
use futures::StreamExt as _;
use zaino_mempool::MempoolUpdate;

// Subscribe BEFORE snapshotting (consistency contract), then reconcile from current().
let mut updates = std::pin::pin!(mempool.mempool_updates());
let mut set = mempool.snapshot();

while let Some(update) = updates.next().await {
    match update {
        MempoolUpdate::Added { entry, .. }   => { /* apply delta */ }
        MempoolUpdate::Removed { txid, .. }  => { /* apply delta */ }
        MempoolUpdate::Reset { .. }          => set = mempool.snapshot(), // batch boundary
        MempoolUpdate::Lagged { .. }         => set = mempool.snapshot(), // fell behind: resync
        MempoolUpdate::Closing { .. }        => break,
    }
}
```

The raw `mempool.subscribe_updates()` receiver is available for advanced use; it
surfaces the same lag as `RecvError::Lagged`, which is easy to drop silently, so
reach for `mempool_updates()` unless you need the receiver.

## Layer the coherence service (feature `tip_aware_mempool`)

`CoherenceService<M, N>` consumes a `Mempool` core (`M`) and an `NfsEpochObserver`
(`N`); it re-uses the core's already-fetched, tip-tagged set and issues no source
reads of its own.

```rust
use zaino_mempool_service::CoherenceService;

let coherence = CoherenceService::spawn(
    core.subscriber(),  // the Mempool port
    nfs_observer,       // : impl NfsEpochObserver
    MempoolConfig::default(),
    cancel.child_token(),
);
let coherent = coherence.subscriber(); // CoherentSubscriber
```

Validator-only (no NS; synthesize the epoch from the validator tip, single-tip
freeze/thaw): `CoherenceService::spawn_validator_only(core.subscriber(), config, cancel)`.

Give the observer a `subscribe_epoch_changes` signal if you can. The layer also
reconciles on its poll tick and on the core's change feed, but the tick alone
leaves tip-coherent reads frozen for a poll interval after every block.

Pass **clones of one** `MempoolConfig` to the core and the coherence service: the
memory bound is a shared atomic, so two independent `default()`s would silently
give them separate knobs.

## Tip-coherent reads and the stream

Gate combined reads on the coherent view matching the caller's NS epoch:

```rust
use zaino_mempool::TipAwareMempool as _; // bring the port method into scope

let view = coherent.coherent_snapshot();
if view.is_valid_for_snapshot(caller_epoch) {
    if let Some(entry) = view.get(&txid) { /* serve from the coherent mempool */ }
}

// Stream the mempool until the tip changes, then it closes:
if let Some(stream) = coherent.stream_transactions_until_tip_change(Some(caller_epoch)) {
    let mut stream = std::pin::pin!(stream);
    while let Some(item) = stream.next().await {
        match item {
            Ok(tx_bytes) => { /* forward tx; `Bytes`, so no copy */ }
            // Fell behind the event feed: the set delivered is INCOMPLETE.
            // Surface a retryable error; never treat this as a clean end.
            Err(error) => { /* report and re-open against a fresh snapshot */ break }
        }
    }
} // None => caller's tip is stale; re-snapshot and retry
```

`stream_transactions_until_tip_change` yields the coherent set then each subsequent
addition and closes on a *new* epoch (a transient freeze keeps it open on the last
coherent set). It is a `TipAwareMempool` port method, so consumers can program
against the port rather than the concrete type.

## Routing summary (as wired in `zaino-state`)

| RPC / read | Layer |
|---|---|
| `getrawmempool`, `getmempoolinfo`, `GetMempoolTx` | core (`MempoolSubscriber`) |
| `get_raw_transaction`, `get_transaction_status` | coherence (`CoherentSnapshot`) |
| `get_mempool_stream` | coherence (`stream_transactions_until_tip_change`) |

## Observing it

Both loops log under their crate target, so `RUST_LOG=zaino_mempool_service=debug`
turns them up without touching the rest of the stack (see `docs/logging.md`).

At the default `info` level you see only the edges that change what the mempool
is serving:

- `warn` — the set went incomplete. Carries `cause` (the validator port that
  failed, or `tip_unstable` when the tip will not hold still long enough to tag
  a set against), the underlying `error`, and the `tx_count` still being served.
- `info` — the source recovered and polls are being applied again. Every `warn`
  above is eventually closed by one of these.
- `debug` (coherence) — freeze and thaw, carrying the `FreezeReason`. These are
  at `debug` rather than `info` on purpose: every block freezes coherence
  briefly, so at the default level a healthy node would log one line per block.
  Turn them up when you want to know *why* a freeze happened; the escalation
  `warn` (and the `zaino.mempool.coherence_frozen_seconds` gauge) is what tells
  you a freeze has outlasted normal thaw.

Nothing is logged per poll or per reconcile. At a sub-second cadence that would
be noise, so a validator that stays down produces one `warn`, not thousands; turn
the level up to `debug` to confirm it is *still* failing and why.

The two loops each run inside one long-lived span — `mempool_poll_loop` and
`mempool_coherence_loop` — which is what `ZAINOLOG_FORMAT=tree` groups on.

For alerting rather than reading, prefer the status and the freeze clock:
`MempoolSubscriber::status()`, `CoherentSubscriber::frozen_for()`, and the
`zaino.mempool.coherence_frozen_seconds` gauge `zainod` exports.
