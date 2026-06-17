# BUG — StateService backend never finishes startup: TrustedChainSync non-finalized reconnect storm

**Reported by:** experiments Claude (host `pua`), 2026-06-17.
**Severity:** blocker for the StateService backend — `zainod` never starts serving.
**Captured evidence:**
- `captures/runner-zainod-state-STUCK-20260616-233306.log` (zaino, state backend)
- `captures/zebrad-grpc-20260616-233306.log` (host zebrad serving the indexer gRPC)

## Summary
With `backend = 'state'`, `StateService::spawn` opens the zebra-state DB read-only (RocksDB
secondary of the host zebrad) and then **blocks forever** in its "catch up to validator tip" loop.
The catch-up never completes because the **non-finalized** blocks (the most recent ~100, which live
only in zebra's in-memory non-finalized state) never reach zaino: zaino's `TrustedChainSync`
subscribes to the indexer's `non_finalized_state_change` stream, fails to drain it fast enough,
zebra drops it as a *"slow consumer"*, and zaino **immediately re-subscribes with no backoff** —
a ~40/sec reconnect storm. `syncer_height` stays pinned at the finalized tip; `:8137` never opens.

## Environment
- Host zebrad: built from **zebra v5.1.1** with `--features indexer`, DB format `27.0.0+indexer`,
  serving indexer gRPC `127.0.0.1:18230` + JSON-RPC `127.0.0.1:18232`, at mainnet tip (~3,380,770).
- zaino: `optimize_sync` build, `zainod-state.toml` (`backend='state'`,
  `zebra_db_path=/home/pua/zebra-mainnet-seed` opened read-only/secondary,
  `validator_grpc_listen_address=127.0.0.1:18230`).
- The DB was seeded from a quiesced copy of the cluster zebra and migrated to `+indexer` by the
  host zebrad before zaino launched. zebrad is healthy and at tip throughout.

## Symptom (observed)
zaino log, repeating ~1×/sec, `syncer_height` frozen ~99 below the validator:
```
INFO zaino_state::backends::state: ReadStateService syncing with Zebra, syncer_height: 3380671, validator_height: 3380770
```
zebrad log, simultaneously, thousands of times:
```
INFO  zebra_rpc::indexer::methods: client disconnected, dropping non_finalized_state_change task
WARN  zebra_rpc::indexer::methods: slow consumer, dropping non_finalized_state_change stream
```

**Measured churn:** in a 3.5-minute window (06:29:28 → 06:33:04): **9,119** `client disconnected`
+ **77** `slow consumer` drops on the `non_finalized_state_change` stream (~43 reconnects/sec, and
accelerating: 2,645 at the 2-min mark → 9,119 at 3.5-min). `syncer_height` advanced exactly **1
block in ~2 minutes** (only via normal finalization), so the ~99-block non-finalized gap never
closes. zaino stays alive but never opens its gRPC (`:8137`).

## Root cause — two compounding defects (both in `zebra-rpc`, exercised by the State backend)

**1. zaino is a slow consumer of the non-finalized stream (the primary defect).**
`zebra-rpc/src/indexer/methods.rs:84` `non_finalized_state_change` streams blocks to the client via
a **bounded** `mpsc` with `try_send`; on `Full` it logs `slow consumer` and **returns (drops the
stream)** — `methods.rs:~132`. So whenever zaino's consumer can't keep pace with the burst (the
initial subscription replays the whole non-finalized set, ~100 blocks), zebra tears the stream down
instead of applying backpressure. The non-finalized blocks are never fully delivered.

**2. zaino re-subscribes with no backoff on a stream drop (the amplifier).**
`zebra-rpc/src/sync.rs:143-178` `TrustedChainSync` read loop: a slow-consumer drop arrives on the
client as `message().await == Ok(None)` (`:162`), which sets `non_finalized_blocks_listener = None`
and `continue`s → the loop top immediately re-subscribes (`:144-147`). The `POLL_DELAY` sleep
(`:152`) is applied **only** to subscribe *errors* (`Err` at `:150`), **not** to stream-ends
(`Ok(None)` at `:162`, `Err` at `:167`, malformed at `:174`). So a recoverable hiccup becomes a
tight ~40/sec reconnect storm that hammers zebrad and prevents either side from ever stabilizing.

**Why it never recovers:** zaino `backends/state.rs:223-250` gates startup on
`server_height == syncer_height`, where `syncer_height` is `ReadRequest::Tip` (finalized DB +
whatever non-finalized blocks TrustedChainSync has applied). Because (1) keeps the non-finalized
blocks from ever being applied, `syncer_height` never reaches the validator's non-finalized tip, so
`StateService::spawn` blocks indefinitely and `zainod` never serves.

## Suggested fixes (in priority order)
1. **Backoff on stream-end, not just subscribe-error** (`sync.rs`): apply `POLL_DELAY` (or
   exponential backoff) on the `Ok(None)` / `Err` / malformed arms at `:162-178`, not only on the
   subscribe `Err` at `:150`. This alone stops the 40/sec storm and the load on zebrad. *Necessary
   but not sufficient* — it won't deliver the non-finalized blocks, just stop the thrash.
2. **Don't drop on `Full` — apply backpressure** (`indexer/methods.rs`): use `send().await`
   (bounded, awaits capacity) instead of `try_send()` + drop, or raise the channel capacity to
   comfortably hold the full non-finalized set (≥ the ~100-block reorg limit) plus headroom. The
   "slow consumer → drop the whole stream" policy is fundamentally incompatible with a consumer that
   must receive *every* block to stay consistent.
3. **Make the client a faster consumer / decouple ingest** (`sync.rs`): drain `message()` into a
   local queue before doing per-block work (`decode`, `any_chain_contains`, commit), so network
   reads don't stall behind processing and trip the server's `try_send(Full)`.

These are **`zebra-rpc` (upstream zebra v5.1.x) code**, shared by the indexer server (zebrad) and
the `TrustedChainSync` client (zaino via `init_read_state_with_syncer`). Fixing inside the
`optimize_sync` zaino tree requires patching/forking the `zebra-rpc` dependency or upstreaming to
ZcashFoundation/zebra. Fix #1 is the smallest change that stops the storm; #2 is required for the
backend to actually function.

## Reproduce
1. Host zebrad (v5.1.1 `--features indexer`) at mainnet tip, serving indexer gRPC + JSON-RPC.
2. `zainod start --config zainod-state.toml` (`backend='state'`, pointing at that gRPC/RPC).
3. Observe: zaino loops `ReadStateService syncing with Zebra` with `syncer_height` ~99 below the
   validator; zebrad logs a high-rate stream of `slow consumer` / `client disconnected` on
   `non_finalized_state_change`. zaino never opens `:8137`.
