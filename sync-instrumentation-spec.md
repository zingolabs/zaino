# Instrumentation spec — localize the sync chokepoints (refined)

**Principle:** in-band, always-on, allocation-free (relaxed atomics + RAII guards on the
existing `SyncPhaseMeter`), surfaced in the 10s reporter line. Turn each *suspected*
chokepoint into a *measured* one before changing code.

## Measured context (load-bearing)
- Steady-state sandblast (~1.73M) phase split: **fetch-wait ~75%, build ~24%, write ~0%**.
- Per-block latency zaino sees: **getblock ~293 ms (53 KB resp) + z_gettreestate ~141 ms
  (2.5 KB resp)**; build ~28 ms.
- Direct-to-zebrad, *during the live sync*: **getblock 6–8 ms, z_gettreestate 5 ms**. So
  **~97% of "fetch" time is inside zaino**, not the validator/network.
- ~9 open connections to :8232, K=8, but effective fetch concurrency ≈ 4.
- A 6 GiB commit once froze the pipeline ~95 s (height flat, `write 0%` — meter was blind to
  in-progress writes); a 1,387-block sandblast commit took ~2 s (`write 15%`). Write is
  **~1% amortized** in steady state; the 95 s freeze was a one-time **cold-resume** event.

## The reframe this forces
The bottleneck is **inside zaino, per-call** — not the validator, and (given `buffered`
front-loads K) not "we aren't issuing K." The wire baseline already half-splits the two shapes:
- **getblock 293 ms / 53 KB** — scales with payload → looks **CPU-bound deserialize**
  (hex-decode + block parse + hash). If so, raising K / restructuring / connector gauges are
  near-zero ROI; the lever is decode cost (fewer bytes fetched, faster decode, more workers).
- **z_gettreestate 141 ms / 2.5 KB but 5 ms on the wire** — *not* payload-scaled → ~136 ms of
  **non-work**: client-call overhead or **poll-starvation** (a `buffered` future sits
  ready-but-undriven while the consumer builds/writes; `buffered` only advances when the
  consumer polls the stream). If so, the fix is **structural** (drive fetches independently of
  consumer polling), not a knob.

So the live question is **CPU vs wait**, not concurrency. That reorders the ROI below.

## HIGH — settle CPU-vs-wait first (cheapest, decisive)
1. **Zero-code: process CPU% during sync** (`top` / `pidstat -p <pid> 1`). Cores pegged while
   blk/s is low → **CPU-bound** (getblock deserialize is the wall). Cores idle while blk/s is
   low → **wait-bound** (poll-starvation or a client lock). One reading splits the hypotheses
   before any code.
2. **One-shot A/B: `buffered(K)` → independently-driven tasks** (`FuturesUnordered` /
   `JoinSet`, kept height-ordered into the writer). If z_gettreestate's 141 ms collapses toward
   its 5 ms wire time, it was poll-starvation and spawning is the fix. This is a cheap *test of
   the dominant hypothesis* — higher ROI than any new gauge. (Correctness gates below.)

These two answer "where inside zaino" with near-zero instrumentation.

## MEDIUM — in-progress write attribution (mostly already shipped)
The live commit indicator (`⚠ commit Ns in flight`) + cumulative write% already added settle
the "is write a hidden major cost" hypothesis: cumulative write% amortizes the spiky
completion-accounting and reads the true **~1%**. **Done.** The full `current_phase: AtomicU8`
+ `phase_since` state-machine generalization is **[OPTIONAL]** polish — add only if the phase
split proves untrustworthy.

## LOW — fetch in-flight gauge (was #1; downgraded)
`buffered(K)` front-loads K futures and holds them across *both* awaits, so a pipeline-level
in-flight count sits at **≈K by construction** — the "max ≪ K" branch essentially never fires
and a *max* gauge is uninformative. The effective-4 lives *below* the pipeline (client/CPU),
which this can't see. Keep only if reworked to a **time-weighted average** in-flight AND paired
with the CPU-vs-wait read above; otherwise skip.

## LOW / gated — connector RPC in-flight (was #4)
Only if CPU-vs-wait shows **wait-bound at the client** (not poll-starvation): atomic inc/dec
around the jsonrpsee `send_request`, surfaced on the `FetchService` source. jsonrpsee is a
black box for true wire-time, so this gauge + the 6 ms curl baseline is the practical
localization. Gated; no work now.

## DROP — per-commit decomposition (was #3)
Lowest ROI now. It calibrates batch tuning, but we're ~97% fetch-bound inside zaino with write
~1%, and the 95 s stall was a one-time cold-resume event the spec itself says not to act on. The
`/proc/self/stat` majflt read also carries a parsing footgun — field 2 (`comm`) can contain
spaces/parens, so naive whitespace-splitting for field 12 is wrong; must split on the last `)`.
Fragile code for a parked decision. Revisit only if write% becomes material.

## Degradation audit (none of the *kept* gauges slows sync)
- All gauges are **relaxed atomics + RAII**: a handful of ops per block/fetch, negligible
  against ~430 ms/block fetch + ~28 ms build. No added `.await`, lock, clone, or allocation on
  the hot path.
- The reporter is a separate 10 s task; one log line per window.
- **The one rule to hold:** atomics only — **no `Mutex` on the consumer/fetch path** (a hot-path
  lock is the single thing here that *would* degrade), and keep any `/proc`/syscall read
  **off the per-block path** (per-commit only — and we're dropping that one).
- **Not instrumentation — validate as a code change:** the `buffered`→spawned A/B alters
  behavior. Before keeping it, confirm (a) the writer still receives **height-contiguous**
  blocks (ordering preserved into the batcher), and (b) concurrency stays **bounded** (cap
  in-flight so memory / blocks-in-flight don't grow on fat sandblast blocks). The existing
  out-of-order golden test + bounded-concurrency test cover both — extend them, don't ship blind.

## Revised priority
1. **Process CPU%** (zero-code) — CPU vs wait.
2. **`buffered`→spawned A/B** — tests poll-starvation directly; cheap, and it's a candidate fix.
3. In-progress write attribution — **already shipped** (lighter form); heavy generalization optional.
4. Everything else (in-flight gauges, per-commit decomposition) — low / gated / dropped.

## What NOT to do yet (until CPU-vs-wait is settled)
- **Don't raise K** — if CPU-bound, it does nothing; if poll-starved, spawning (not K) is the fix.
- **Don't shrink the batch (6→2 GiB)** — gives back sorted-write locality; the 95 s stall was
  one-time cold-resume, not a recurring sandblast cost.
- **Don't build parallel build** — build is ~24% under a ceiling well above current throughput;
  fetch dominates and is the only lever that moves the wall now.
