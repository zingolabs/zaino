# EXP-0001: Rebuild-and-cutover finalised-state schema upgrades

> 🧪 **Design exploration — NOT a decision, NOT scheduled.**
> Speculative thinking recorded for posterity. It binds nothing, supersedes
> nothing, and is plausibly never revisited — much less implemented. No code or
> ADR may depend on it. If it ever graduates to a real decision, a new ADR is
> created and this file's status is set to `Promoted → ADR-NNNN`.
> See `docs/explorations/README.md` for the category convention.

## Status

Exploratory

(Formerly drafted as ADR-0002; reclassified — it is exploratory, not a decision.)

## Context and decision

The finalised-state database carried a stepwise migration framework
(`MigrationManager`, the `Migration` trait, `MigrationStep`, `MigrationType`,
per-version `migrate()` bodies, and a `get_migration` registry) that mapped one
`DbVersion` to the next and classified each step as Patch / Minor / Major. It is
powerful but heavy: every schema change needs a hand-written, individually
crash-safe migration step, and the in-place backfills (e.g. v1.1→v1.2) are some
of the most intricate code in the subsystem.

We are replacing it with a single, uniform mechanism: **rebuild-and-cutover.**

At startup the process compares the loaded DB's **schema hash** against a
build-embedded **ordered set of known schema hashes** (oldest → current):

- `== current` → open and serve normally.
- in the **known-older** set → the loaded DB is *older*; serve from it while a
  fresh DB at the current schema is **rebuilt from the validator** in the
  background; **cut over** when the rebuild catches up.
- **unknown** (newer than this build, or foreign/corrupt) → refuse to open it
  rather than rebuild over it, so a newer or foreign DB is never destroyed.

There is no in-place migration and no change-type classification. Every older
schema — even a metadata-only bump — is a full from-genesis rebuild fed by the
validator.

During a rebuild the **old DB stays live**: it keeps taking live tip writes and
serves *all* reads. The new **building DB** bulk-syncs from the validator in its
own directory. When it reaches the current finalised tip the **active pointer**
is flipped atomically and the building DB becomes the serving DB; the old DB is
then deleted. New-schema-only capabilities are unavailable until that instant,
then switch on atomically.

## On-disk layout and the cutover commit point

Each database lives in a directory keyed by its schema id:
`<network>/<schema_id>/`. A durable **active pointer** record names the serving
DB's schema id and is the single linearization point of the whole scheme:

- Flipping the pointer (a single atomic, fsync'd write — or atomic rename) *is*
  cutover.
- On restart, the pointer names the authoritative serving DB.

The building DB is wired **outside** the router. A dedicated rebuild task owns
it and bulk-syncs it directly (not through the router's live write path, which
keeps appending to the old primary). At cutover the task atomically flips the
pointer, calls `Router::replace_primary(building)`, and deletes the returned old
DB. The `Router` therefore stays a minimal primary(+cold-start-ephemeral)
dispatcher; `EphemeralMode::Full` (the migration primary-freeze) is removed,
while `EphemeralMode::ReadOnly` is kept for cold-start.

**No write freeze at cutover.** This is the finalised state, whose tip advances
only when a block finalises (~once per block interval), so at catch-up the
target is quasi-static. The live write path is a single serialized loop that
reads the active-pointer DB's tip and appends `tip + 1`; flipping the atomic
pointer between iterations keeps writes contiguous, and any single-block residual
is closed by the normal live path on the now-active DB — within the finalised
state's normal sub-block lag.

## Considered options (rejected)

- **Keep stepwise migrations, or a smaller in-place fast path.** Rejected for
  simplicity: the fast path is exactly the classification (Patch/Minor/Major)
  and the per-step crash-safety we are trying to delete. The cost we accept is
  that a trivial schema change now triggers a full resync.
- **Rebuild but source reusable data from the old DB** (validator only for
  genuinely-new data). Cheaper reshapes, but the builder must read the old
  schema while writing the new — reintroducing exactly the per-change reasoning
  the rebuild model removes. Rejected.
- **Hash-equality only (no ordering).** A content hash cannot tell *older* from
  *newer/foreign*, so a downgraded binary could rebuild over — and destroy — a
  newer DB. Rejected in favour of the ordered known-schema set.
- **Validator passthrough for new-schema capabilities during the rebuild
  window.** Would make a new feature live immediately, but keeps passthrough
  machinery on the hot path and risks semantics that diverge from the eventual
  indexed answer. Rejected: new capabilities wait for cutover.
- **Per-height read routing across the two DBs.** The old DB is always ≥ the
  building DB until catch-up, so it collapses to "old serves" anyway. Rejected
  as machinery for no gain; cutover is whole-DB.
- **Startup GC of stray directories.** Rejected in favour of **delete-after-flip
  only** (below).
- **A first-class "building" slot inside the `Router`.** More observable, but
  more router machinery — the opposite of the goal. Rejected in favour of
  owning the building DB outside the router and swapping via `replace_primary`.

## Consequences

- **A metadata-only schema bump now costs a full mainnet resync.** This is the
  surprising part: there is no cheap path. It is paid in the background while the
  old DB serves, so availability is preserved, but it adds resync time and
  validator load on every schema change. Schema changes are rare, which is what
  makes the trade acceptable.
- **A new feature stays dark for the rebuild duration** (potentially hours on
  mainnet) and then appears atomically at cutover.
- **~2× disk *and* a RAM ceiling during a rebuild** — both DBs exist at once, so
  the pre-flight gate must estimate **two** resources, not one:
  - **Disk** — ~2× the populated footprint (old + building DB).
  - **RAM** — the building DB's from-genesis rebuild ends with a full txout-set
    accumulator build that holds an in-memory *spent-set* of roughly
    `spent_outpoints × ~128 B / shards`. The shard count must be chosen so a
    single shard's spent-set fits **measured free RAM minus the co-resident old
    DB's working set** — *not* the `sync_write_batch_size` write-buffer config the
    standalone builder reuses as its budget. That default (32 GiB) exceeds a
    memory-constrained host's RAM (e.g. a 16 GiB pod), so the auto-sharder would
    pick too few shards and **reproduce the original accumulator OOM** the sharding
    was meant to prevent. Sizing the shard budget from measured headroom (the
    rebuild task passes it explicitly; the builder is already parameterized on the
    budget) is what keeps the build within RAM while the old DB keeps serving.

  If either estimate fails, the rebuild is not started, a warning is surfaced, and
  it is re-checked periodically.
- **Rebuild failure is non-fatal: degraded-but-serving.** Because the old DB is
  intact, a rebuild that cannot progress (validator down, repeated errors,
  insufficient disk) never takes serving down. It retries with backoff under a
  non-fatal degraded status + metric and keeps serving the old schema
  indefinitely. This is a deliberate change from the old behaviour, where a
  failed background migration escalated to `CriticalError`.
- **Resume.** A building DB resumes iff a *current-schema* directory exists with
  readable metadata and a recorded sync height; it continues from that height,
  trusting the existing fsync-checkpoint + idempotent-write crash-safety to
  absorb a torn tail. A partial of a different / no-longer-current schema, or one
  with unreadable metadata, is not a valid resume target and is rebuilt fresh.
  This applies equally to the initial sync (no prior serving DB) and a rebuild.
- **Delete-after-flip only; no startup GC.** The obsoleted old DB is deleted
  right after a successful flip (plus read-handle drain). A crash between flip
  and delete, or a stale build left by a double-upgrade, leaks disk but stays
  correctness-neutral — *which holds only because* the active-pointer write is
  atomic + durable (so restart never reads a torn cutover) and resume keys
  strictly on the current schema id (so a no-longer-current partial is never
  mistaken for the target).
- **Cold start keeps the validator passthrough.** On a first run with no DB, the
  passthrough serves reads during the initial from-genesis sync so the service
  is available immediately; the freshly built DB becomes the serving DB on
  completion. The passthrough survives only for cold-start availability and the
  standalone `ephemeral` config mode — never during a rebuild.
- **Removed:** `MigrationManager`, `Migration`, `MigrationStep`, `MigrationType`,
  every per-version `migrate()` body, the stepwise `get_migration` registry,
  `MigrationStatus`, and `EphemeralMode::Full`. **Retained and reused:** the
  `Router` (`replace_primary`), the ephemeral passthrough +
  `EphemeralMode::ReadOnly`, `DbV1`, the capability model, `DbReader`, and the
  bulk-sync loop (`write_blocks_to_height`) that now fills the building DB.

## Open question (not yet decided)

Whether *all* sync — initial sync, live finalised tip advance, and rebuild —
should move into its own binary that does nothing but sync, leaving a read-only
serving process that watches the active pointer and re-opens on cutover. LMDB's
single-writer / multi-reader model across processes makes it feasible and would
isolate the resource-heavy bulk-sync phase (cf. the `WRITE_MAP` map-size
constraints) from latency-sensitive serving, at the cost of cross-process
cutover coordination (active-pointer watch and cross-process read-handle drain
before delete-after-flip) and a two-binary deployment. Recorded here so the
design above is understood to be agnostic to that split — the active pointer is
already the inter-process contract it would need.
