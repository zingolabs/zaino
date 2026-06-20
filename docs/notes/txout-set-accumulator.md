# Notes: the finalised txout-set accumulator (and its sharded builder)

Scratch study notes from a grilling session. Not a spec. Source files cited inline.
A quiz-question bank is at the bottom.

## What it is

- The **txout-set accumulator** is the finalised-state portion of `gettxoutsetinfo`
  — UTXO-set **summary statistics** (UTXO/tx/output counts, total zatoshis, a
  serialized-hash commitment), held as a single small `FinalisedTxOutSetInfoAccumulator`
  value: an **XOR commitment + additive counters**.
  - It is **NOT** per-UTXO storage. The output is one singleton DB row
    (`tx_out_set_info_accumulator` table, key `"tx_out_set_info_accumulator"`).
  - Ref: `finalised_source/v1.rs:139-145`, `:284-289`; `types/db/metadata.rs:165-258`.

## Two ways it is brought to the tip

1. **Incremental** (`update_tx_out_set_accumulator_for_range`): applies only the
   delta for a just-written range. O(range outputs) **random** `spent`/prev-output
   lookups → page-faults once the DB exceeds RAM. Capped at
   `ACCUMULATOR_INCREMENTAL_MAX_GAP = 1000` blocks (`v1.rs:159-169`).
2. **Full from-genesis rebuild** (`build_tx_out_set_accumulator_blocking`): used
   for the first build or any gap > 1000 blocks. Sequential, **zero random reads**.
   This is the OOM path (#1260) and the one every EXP-0001 rebuild ends with.

Both produce the **identical** accumulator at the tip; the watermark
`_tx_out_set_accumulator_built_height` lets readers detect a stale accumulator
(watermark < db_tip) and dispatch picks the cheap path (`v1.rs:146-168`).

## What a "shard" is

- A shard is a **band of the first byte of the *creating* txid** (`T_c[0]`).
  For `shards` total, shard `i` owns `[i*256/shards, (i+1)*256/shards)`
  (`transparent_address_history.rs:1766-1773`).
- `shards = 1` → one band over all 256 values (whole set, one pass).
- Sharding is a **memory ↔ passes knob only**: partials recombine by XOR + additive,
  so the result is **independent of shard count** (`...:1764`, `:1754`).

## The builder, per shard (`build_tx_out_set_accumulator_blocking`, ...:1758-1880)

1. **Spent-set pass:** scan the `spent` table once; insert into an in-memory
   `HashSet<Box<[u8]>>` only keys whose `key_bytes[1]` is in the band
   (`:1782-1790`). `key_bytes[1]` is `prev_txid[0]` because the `spent` key is
   `Outpoint::to_bytes()` = `[1-byte version tag][32-byte prev_txid][4-byte index]`.
2. **Block-scan pass:** walk `transparent`+`txids` tables height-ascending; for each
   tx, `if !in_shard(txid.0[0]) continue` (`:1843`); for each spendable output build
   `Outpoint::new(txid.0, out_index).to_bytes()` and test
   `spent_set.contains(full_key)` (`:1857-1859`). Not-in-set ⇒ still unspent ⇒ fold
   into `shard_acc`. `is_unspendable_tx_out` outputs are skipped (`:1853`).
3. XOR+additive-combine `shard_acc` into `total`.

## Cost model

- **Time** ≈ `shards` sequential full passes over genesis→tip block data
  (sequential I/O, no page-fault storm — that's the whole reason the rebuild path
  exists vs the incremental random-lookup path).
- **Memory** ≈ `(total_spent_entries × 128 B) / shards`.
  - Per-entry cost: the `Box<[u8]>` of the ~37-byte spent key **plus** HashSet/alloc
    overhead (fat pointer, alloc header, load-factor slack). The code estimates
    `SPENT_SET_ENTRY_BYTES_ESTIMATE = 128` B (#1263 diff) — conservative (real ~60–90 B);
    over-estimating only *adds* shards, never under-bounds.
  - NB the in-RAM set is the **spent** set (lifetime spent outpoints, grows with chain
    history), **not** the current UTXO/unspent set. That's why it's the RAM hog.
  - `/shards` holds because txids are hash-uniform ⇒ each first-byte band gets
    ≈ 1/shards of entries.
- **Reported** (#1263 body, NOT independently derived): 1 shard at mainnet tip
  exceeded 16 GiB → exit 137 on a 16 GiB pod.

### THE BUG: shard count is sized from a write-buffer config, not from RAM

#1263's actual code (verified from the PR diff, NOT yet merged on this branch):
- `accumulator_build_shards(budget_bytes)` = `ceil(spent_count × 128 / budget_bytes).clamp(1, 256)`
  (`ACCUMULATOR_BUILD_MAX_SHARDS = 256`; old pinned `ACCUMULATOR_BUILD_SHARDS = 1` removed).
- The function is **parameterized** on `budget_bytes` (testable: test feeds `u64::MAX` and `1`),
  but **all three production call sites pass `sync_write_batch_size.to_byte_count()`** verbatim
  (diff l.332-333, 426, 451). So in production the accumulator RAM ceiling **IS** the
  write-path batch knob — same value, not just "conceptually reused."

**Why it's a bug** (`budget` is the *denominator* — bigger budget ⇒ fewer shards ⇒ more RAM/shard):
- `sync_write_batch_size` is a **write-buffer width** ("block bytes buffered before an LMDB
  commit"); its own doc says peak RAM is *"this budget **plus** dirty pages"* — it was never
  the RAM ceiling, only a component. #1260 **raised its default 4 → 32 GiB**.
- The guarantee #1263 claims — *"in-RAM set never exceeds the budget"* — is **true but vacuous
  at default**, because `budget` (32 GiB) > a constrained host's RAM (16 GiB):
  - est ~20 GiB → `ceil(20/32) = 1 shard` → ~20 GiB → **OOM, same as #1260**.
  - est ~40 GiB → `ceil(40/32) = 2 shards` → ~20 GiB/shard → **still OOM**.
  - to fit 16 GiB you need `budget ≈ 12 GiB` → 4 shards. Operator must **manually lower** it.
- So **at default config the patch does not fix the reported 16 GiB-pod scenario** — and the
  4→32 GiB default bump made auto-sharding *later* to engage, i.e. worse for small hosts.
- **Correct fix:** size shards from **measured free RAM**, not `sync_write_batch_size`:
  `budget = measured_free_RAM × safety_fraction − co_resident_footprint`, injected through the
  existing `budget_bytes` param (one-line call-site change). The `− co_resident_footprint` term
  is the EXP-0001 add-on (live old DB co-resident during a rebuild); the `measured_free_RAM`
  term is the standalone fix.

## Back-of-envelope with live mainnet numbers (Blockchair API, height 3,384,176, Jun 2026)

| Quantity | Value | Note |
|---|---|---|
| Total transparent outputs ever | **189,520,271** (~189.5M) | upper bound on the spent set |
| Total transactions | 17,754,412 (~17.75M) | ≈ `txids` entry count |
| Block height | 3,384,176 | matches PR sync log (~3.38M) |
| Current UTXO (unspent) set | not exposed | small — see cross-check |

**The number that drives RAM is the *spent* set, not the UTXO set.** The in-RAM `HashSet`
holds lifetime *spent* outpoints = `total_outputs − UTXO`. We don't have UTXO directly, but
two estimates agree: most sandblast-era outputs are spent ⇒ `spent ≈ 185M`; and #1260's
empirical "> 16 GiB at 1 shard" cross-checks: `185M × ~90 B real ≈ 15.5 GiB`. So the UTXO
remainder is small and `spent ≈ total_outputs`.

**RAM, and the bug quantified:** per-shard ≈ `spent × 128 B / shards`.
- 1 shard: `185M × 128 B ≈ 22 GiB` (code estimate); ~15.5 GiB real.
- At the shipped default `budget = sync_write_batch_size = 32 GiB`:
  `shards = ceil(22 / 32) = 1`. **At today's chain size the auto-sharder picks ONE shard
  at default config — functionally identical to the removed pinned `=1` that caused #1260.**
  It stays 1 shard until the spent set exceeds 32 GiB ≈ 268M entries ≈ **1.45× today's chain**
  (years away). On a 16 GiB pod: 1 shard → ~16–22 GiB → **OOM, reproduced**.
- Correctly budgeted (10 GiB headroom): `ceil(22/10) = 3 shards → ~7.3 GiB/shard` — fits.

**Time needs MORE than the count.** Per shard the builder re-scans the **entire** `spent`
table (iterates all, `continue`s out-of-band, `:1785`) **plus** the entire `transparent` +
`txids` tables:
```
total_work = shards × ( |spent| + |transparent| + |txids| )   entries, each decoded + checksum-verified
```
The output count gives RAM but **cannot** give wall-clock. To compute time we additionally need:
1. **On-disk byte sizes** of `spent` / `transparent` / `txids` (we have entry *counts*
   ~185M / ~189.5M / ~17.75M, not bytes/entry on disk).
2. **Measured scan+decode throughput** — dominated by per-entry deserialize + checksum
   `verify()` (`:1811`, `:1826`), CPU-bound, not raw disk if page-cached. Must be measured.
3. **Shard count** — the memory↔time multiplier.

**Key asymmetry:** fixing the OOM (more shards) makes time **worse** — each extra shard is
another full redundant re-scan of the spent table. Memory fix and time cost pull opposite
ways, which is exactly why the budget must come from *measured* RAM, not a 32 GiB default:
over-shard ⇒ waste hours, under-shard ⇒ OOM.

## The co-sharding correctness invariant (the subtle part)

**Claim:** if an output is spent at/below the tip, its spend-record is in the **same
shard** as the output — so a single shard pass decides spent/unspent with **zero
cross-shard lookups**.

**Why it holds — by construction, NOT a runtime check:**
- The discriminator on *both* sides is the **creating** txid `T_c` (the spending tx
  `T_s` never enters shard math). An `Outpoint`'s `prev_txid` *is* the creating txid of
  the output it references — same 32 bytes, by definition (`types/db/legacy.rs:658-688`),
  not a hash coincidence.
- The **same `txid.0` value** both (a) gates the shard at `:1843` and (b) builds the
  lookup key at `:1857`. And step 1 admitted that output's spend-record to the same
  band under the identical `in_shard` predicate. Same value, same closure, same pass.
- Membership test is on the **full 37-byte key** (`:1859`), so no first-byte-collision
  false positives; the shard byte only selects *which pass sees the entry*.

**The fragile seam:** step 1 reads the shard byte by **raw offset** (`key_bytes[1]`,
guarded only by `len() < 2`); step 2 reads it **typed** (`Outpoint::new(...).to_bytes()`).
Coupled by an **unasserted layout assumption**. If `Outpoint` serialization ever shifted
`prev_txid` off offset 1, step 2 tracks it / step 1 does not → spend-records misassigned →
`contains` misses → **spent outputs counted as unspent → accumulator over-counts**, with
no error raised. Backstops: (a) equivalence test
`tests/.../v1_1_to_v1_2.rs:400` (only catches it if run multi-shard); (b) the schema hash
(BLAKE2b of `db_schema_v1.txt`) flips on any layout change → under EXP-0001 forces a full
**rebuild** at the new schema rather than an in-place reinterpret. So rebuild-and-cutover
*insures* this coupling cross-schema; intra-schema it's honor-system. Suggested hardening:
`debug_assert!(key_bytes[1] == Outpoint::decode(key).prev_txid()[0])`.

## Relevance to EXP-0001 (rebuild-and-cutover)

- Every schema bump → full from-genesis rebuild → ends with the **full** accumulator
  build (gap ≫ 1000). So the expensive event moves from "cold start only" to
  "**every upgrade**".
- During a rebuild the old DB stays **live** (same process) while the building DB runs
  this build, so peak RAM = `old-DB working set + accumulator build`. The ADR pre-flight
  gate checks **disk** (~2×), not **RAM** → an accumulator OOM would take *serving* down,
  contradicting the "rebuild failure is non-fatal" guarantee.
- Memory is the `/shards` knob (clamp 256), so OOM is avoidable **in principle** — but
  **NOT at the shipped default**, because the formula is handed `sync_write_batch_size`
  (32 GiB) as its budget and so *chooses too few shards* on a 16 GiB host (see "THE BUG"
  above). Avoiding OOM requires the building task to pass a budget derived from
  **measured free RAM − old_DB_footprint**, not the write-buffer config.
- The unavoidable inheritance is **time** (the extra sequential passes per shard), which
  the ADR already owns ("a metadata-only bump costs a full mainnet resync").

---

## Quiz bank (to ask later)

1. What does the accumulator actually store — and what does it deliberately *not* store?
2. Two paths to bring it to the tip; what's the cutoff between them and why that number?
3. Define a shard. What byte, of *which* of the three txids involved in a spend?
4. Why is the spending transaction `T_s` irrelevant to shard assignment?
5. State the cost model (time and memory) as a function of `shards`. Why `/shards`?
6. Where does the 16 GiB-at-1-shard figure come from — derived or reported?
7. The co-sharding invariant: state it, then explain how it's *enforced* (trick: it isn't,
   by a check). What single value makes it airtight?
8. The fragile seam: how do step 1 and step 2 reach the shard byte differently, and what
   breaks if `Outpoint` serialization changes? What's the failure *symptom* in the output?
9. Why does EXP-0001's rebuild-and-cutover make the raw-offset shortcut *safe to keep*?
10. Why is bug-C's *time* cost unavoidable under EXP-0001? What gate is missing from the
    ADR pre-flight?
11. THE BUG: `accumulator_build_shards(budget_bytes)` is parameterized, but what value do
    all three production call sites pass? Why does that make the default config reproduce
    the #1260 OOM on a 16 GiB host? State the correct budget source.
12. `budget` is the denominator of the shard formula — so does a *bigger* budget give more
    or fewer shards, and more or less RAM per shard? Why did 4→32 GiB make it *worse* for
    small hosts?
13. With ~189.5M total outputs (≈185M spent), what shard count does the default 32 GiB
    budget pick *today*? What does that make the "fix" equivalent to, and roughly how much
    chain growth before it engages?
14. The output count gives you RAM. Name the three things you *additionally* need to
    compute wall-clock time, and explain the memory↔time asymmetry (why fixing OOM costs
    time).
15. Why is the *spent* set, not the UTXO set, the RAM driver? How does #1260's "> 16 GiB"
    let you cross-check the spent count?
