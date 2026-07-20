# `zaino-mempool` implementation audit

A design review of the mempool read-model against Zaino's requirement: serve every
mempool RPC to many concurrent lightwallet clients, coherently and bounded. It
records what was reviewed, what was changed, and what was deliberately *not*
changed (with reasoning).

## Verdict

The architecture is well-suited to the workload. It is split into a tip-agnostic
core (always-live mirror + change feed) and an optional tip-aware coherence layer
(freeze/thaw), so the live reads never block on tip transitions. The serving (read)
path is lock-free and copy-free; the update (write) path is a single low-frequency
poller. The efficiency work below targets the paths that actually matter for many
clients (per-poll fetch weight, bounded feed cost, boundary-only wire conversion)
rather than micro-optimizing the cheap write clone.

## Read / serve path — optimal, unchanged

Reads go through an immutable [`MempoolSnapshot`] published behind an
`ArcSwap`. A `MempoolSubscriber::snapshot()` is a lock-free atomic load returning a
shared `Arc`; `get_transaction` / `contains_txid` are `O(1)` `HashMap` lookups on
that snapshot. Entries are shared `Arc<MempoolEntry>` — no per-subscriber byte
copies (fixes the historical Z-09 amplification). This is the right shape for many
concurrent readers and is kept as-is.

The change feed flows over a bounded `tokio::sync::broadcast` channel, so a slow
client is lagged (dropped forward), never backpressures the writer or other clients
— memory is capped at `event_buffer_len` slots regardless of subscriber count. Two
properties make it safe at scale: (1) the feed is **lossless at the level of
state** — a lagged consumer is told so explicitly (`MempoolUpdate::Lagged`, or the
in-band signal on the `mempool_updates()` stream) and resyncs from the lock-free
`current()`; (2) buffered updates carry **no snapshots** (`Reset` is a sequence-only
batch boundary; coherent `Live`/`Frozen` events carry only `valid_for`/`reason`), so
slots stay tiny and thousands of subscribers cannot inflate retained memory.

## `im::HashMap` (persistent map) — considered and rejected

A persistent/HAMT map makes clone-and-modify `O(1)` via structural sharing, but
makes lookups `O(log n)` with pointer-chasing and poor cache locality, versus
`std::HashMap`'s `O(1)` cache-friendly lookups.

Our workload is **read-dominated with rare, tiny writes**: one update per poll
(~500 ms), and the full-map clone that `im::` would optimize costs single-digit
microseconds at realistic mempool sizes (Zebra's ZIP-401 cost cap ≈ 80 MB ⇒ at
most ~8k transactions; typically hundreds). Adopting `im::` would slow the hot read
path to save microseconds on the cold write path — a bad trade. **Kept
`std::HashMap`.**

## Implemented efficiency improvements

- **Foundational entry, wire shape at the boundary.** `MempoolEntry` holds only the
  full unmined transaction (bytes + protocol metadata) and a `transaction()` parse;
  it carries no RPC/wire forms. The compact / lightclient `RawTransaction`
  conversions are derived on demand at the RPC boundary, which keeps `zaino-mempool`
  free of `zaino-proto` and lets each layer own its own shape. (A shared compact
  cache can return in the boundary/conversion layer if `GetMempoolTx` profiling
  warrants it, without re-coupling the core to proto types.)
- **Re-fetch-free coherence.** The tip-aware layer reuses the core's already-fetched
  set (tagged with the validator tip `V`); it issues no source reads of its own,
  computing freeze/thaw purely from `(core set + source_tip, NS)`.
- **Slimmed feed payloads.** Change-feed and coherent-event slots carry no snapshots
  (see the serve-path note), bounding retained memory under many subscribers.
- **Light diff + verbose-on-additions.** The update loop diffs on the cheap
  `getrawmempool` txid list every poll and fetches the heavier
  `getrawmempool verbose` (for tip-at-entry heights) only when the diff shows
  additions. Steady-state polls no longer pull the full verbose map.
- **Binary-search exclude filter.** The snapshot keeps txids sorted by *reversed*
  bytes, turning the lightwallet suffix-exclude into a prefix match resolved by
  binary range search (`O(excludes·log n)` instead of `O(excludes·n)`). No extra
  index: the existing `txids_sorted` is simply ordered for this.
- **Runtime-adjustable memory bound.** `MempoolConfig::max_cost_bytes` is held
  behind a shared atomic and set via `MempoolService::set_max_cost_bytes` (on the
  service, not the read handles), so the DoS backstop can be tuned at runtime with
  one shared value across the core and coherence services.
- **Zero-copy transaction fan-out.** The entry's bytes are a `bytes::Bytes` buffer
  built once at ingest and carried unchanged to the wire (`RawTransaction.data` is
  generated as `Bytes`), so serving one transaction to *K* streaming clients costs
  *K* refcount bumps rather than *K* copies.
- **Batch-boundary reconciles.** The coherence layer wakes on the change feed's
  `Reset` only, not on each per-txid message: a cleared block of 1,000 transactions
  is one reconcile instead of ~2,001, and `reconcile` re-reads the core snapshot
  wholesale anyway.
- **Incremental totals, tag-only republish.** `cost_bytes` / `raw_bytes` move with
  the delta rather than being re-summed each publish, and a publish that only
  re-stamps the tip tag reuses every collection and holds `mempool_generation`
  steady (bumping it made coherence treat a re-tag as new contents).
- **Guarded before the expensive work.** The tag-stability check also runs before
  the metadata listing and raw fetches, so a poll that a mid-flight block will
  invalidate is abandoned early rather than after paying in full.

## Documented trade-offs (not changed)

- **Full-map clone + re-sort per publish** (`O(n) + O(n log n)`) *on polls that
  change the set*. Negligible at the 500 ms cadence and realistic sizes. If updates
  ever become far more frequent, a persistent map or incremental sorted structures
  could revisit this — but see the `im::` rejection above for why it is not
  warranted today.
- **Two validator-tip reads per poll** (opening tag + stability guard), three when
  there are additions. The opening read cannot be carried over from the previous
  poll — it is also how a tip *change* over an unchanged mempool is detected, which
  the coherence layer needs in order to thaw. `getblockchaininfo` is cheap relative
  to the mempool calls, so the guard is kept as-is.
- **No compact-transaction cache.** `GetMempoolTx` re-parses each transaction into
  its compact form per request. A cache belongs at the boundary (where the wire
  types live), not in the core, and is deferred until profiling warrants it.
- **`get_filtered_entries` output is inherently `O(n)`** (it returns all
  non-excluded entries), so the binary-search match only dominates when a client
  sends a large exclude list; the improvement is real but situational.

## Follow-ups (out of scope here)

- **Compact-form cache at the RPC boundary**, if `GetMempoolTx` profiling warrants
  it (see the trade-off above).
- **Orphaned streamer `JoinHandle`** in the RPC layer (J-01 bug 2) is tracked under
  AP-02 (the generic streaming task-leak class), not here.

## Correctness

No defect was found. Coherence rests on the core tagging every set with the
validator tip it was fetched at (`source_tip`), guarded by a tag-stability check
(the tip must not move across the fetch window, else the poll is discarded) so the
tag and the data are a single-source pair. The coherence layer then computes
freeze/thaw as a pure function of `(core set + source_tip, NS)` — no re-fetch, no
before/after guards — with generation-on-tip-change distinguishing epochs. The core
never freezes, so the live reads stay correct through tip transitions. The
freeze/thaw, change-feed (including the explicit lag signal), and update rules are
exercised by the `zaino-mempool-rpc` test matrix (core and coherence suites, both
feature modes) and the `zaino-state` mockchain integration tests.
