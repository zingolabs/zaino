# `zaino-mempool` implementation audit

A design review of the mempool read-model against Zaino's requirement: serve every
mempool RPC to many concurrent lightwallet clients, coherently and bounded. It
records what was reviewed, what was changed, and what was deliberately *not*
changed (with reasoning).

## Verdict

The architecture is well-suited to the workload. The serving (read) path is
lock-free and copy-free; the update (write) path is a single low-frequency poller.
The efficiency work below targets the paths that actually matter for many clients
(compact-tx reuse, per-poll fetch weight) rather than micro-optimizing the cheap
write clone.

## Read / serve path — optimal, unchanged

Reads go through an immutable [`MempoolSnapshot`] published behind an
`ArcSwap`. A `MempoolSubscriber::snapshot()` is a lock-free atomic load returning a
shared `Arc`; `get_transaction` / `contains_txid` are `O(1)` `HashMap` lookups on
that snapshot. Entries are shared `Arc<MempoolEntry>` — no per-subscriber byte
copies (fixes the historical Z-09 amplification). Deltas flow over a bounded
`tokio::sync::broadcast` channel, so a slow client is dropped, never backpressures
the writer or other clients. This is the right shape for many concurrent readers
and is kept as-is.

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

- **Compact-tx cache (`MempoolEntry::compact_tx`).** `GetMempoolTx` previously
  re-parsed every transaction into its compact form on every call. Entries are
  shared `Arc`s that persist across polls, so a `OnceCell<Arc<CompactTx>>` computes
  the compact form at most once per transaction and reuses it across all clients
  and snapshots. The conversion still lives in the RPC/`zaino-fetch` layer (via
  `MempoolEntry::compact_tx_or_init`); the entry only owns the cache slot. This is
  the largest win under many-client `GetMempoolTx` load.
- **Light diff + verbose-on-additions.** The update loop diffs on the cheap
  `getrawmempool` txid list every poll and fetches the heavier
  `getrawmempool verbose` (for tip-at-entry heights) only when the diff shows
  additions. Steady-state polls no longer pull the full verbose map.
- **Binary-search exclude filter.** The snapshot keeps txids sorted by *reversed*
  bytes, turning the lightwallet suffix-exclude into a prefix match resolved by
  binary range search (`O(excludes·log n)` instead of `O(excludes·n)`). No extra
  index: the existing `txids_sorted` is simply ordered for this.
- **Runtime-adjustable memory bound.** `MempoolConfig::max_cost_bytes` is held
  behind a shared atomic and exposed via `MempoolSubscriber::set_max_cost_bytes`,
  so the DoS backstop can be tuned per-process at runtime; all subscribers and the
  service share the value.

## Documented trade-offs (not changed)

- **Full-map clone + re-sort per publish** (`O(n) + O(n log n)`). Negligible at the
  500 ms cadence and realistic sizes. If updates ever become far more frequent, a
  persistent map or incremental sorted structures could revisit this — but see the
  `im::` rejection above for why it is not warranted today.
- **`get_filtered_entries` output is inherently `O(n)`** (it returns all
  non-excluded entries), so the binary-search match only dominates when a client
  sends a large exclude list; the improvement is real but situational.

## Follow-ups (out of scope here)

- **Config-file wiring.** The runtime bound is adjustable, but `MempoolConfig` is
  still constructed from defaults at spawn (`chain_index.rs`). Threading a `mempool`
  section from `ChainIndexConfig` is a mechanical follow-up (it touches ~10
  `ChainIndexConfig` construction sites, so it was deferred to keep this change
  focused).
- **Orphaned streamer `JoinHandle`** in the RPC layer (J-01 bug 2) is tracked under
  AP-02 (the generic streaming task-leak class), not here.

## Correctness

No defect was found. The two coherence guards (V == NS before and after the
transaction fetch) plus generation-on-tip-change give a sound freeze/thaw model;
the validator-tip-first observation ordering is self-correcting through the guards.
The freeze/thaw and update rules are exercised by the `zaino-mempool` test matrix
and the `zaino-state` mockchain integration tests.
