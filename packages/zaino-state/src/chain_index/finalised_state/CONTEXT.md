# Finalised-state schema & rebuild — context glossary

Glossary for the finalised-state database's schema-comparison and DB-upgrade
domain. Terms only; no implementation detail. The speculative model behind
these terms — rebuild-and-cutover replacing stepwise migration — is explored in
`docs/explorations/EXP-0001-rebuild-and-cutover-schema-upgrades.md`.

## Terms

- **Schema hash** — content fingerprint (BLAKE2b-256) of the on-disk layout
  *description* (`db_schema_v1.txt`), persisted in `DbMetadata` and embedded in
  the build. The authoritative identity of the layout *contract* — the format of
  each table **if present** — not an assertion of which tables physically exist.
  Feature-gated tables (e.g. the transparent address-history index, the
  txout-set accumulator) are described unconditionally in the text and so do not
  change the hash when compiled out; their physical presence is governed by build
  features and their own watermarks, independently of schema identity.

- **Known-schema set** — the build-embedded, *ordered* list of all schema
  hashes this binary recognises, oldest → current. Ordering makes "older"
  decidable from content alone, without a stepwise migration matrix.

- **Loaded DB** — the on-disk finalised-state database the process opens at
  startup, identified by the schema hash in its metadata.

- **Older DB** — a loaded DB whose schema hash is in the known-schema set but
  precedes the current build's hash. Safe to keep serving from while a new DB
  is built.

- **Unknown schema** — an on-disk schema hash absent from the known-schema set
  (a newer build wrote it, or it is foreign/corrupt). Refused, never rebuilt
  over, so a newer or foreign DB is not destroyed.

- **Current schema** — the schema hash this binary builds new databases at; the
  last entry in the known-schema set.

- **Rebuild** — the sole upgrade mechanism: when the loaded DB is older, build a
  fresh DB at the current schema by syncing from the validator/source (from
  genesis, or resuming a partial new DB). There is no in-place migration and no
  change-type classification; every older schema is a full from-source rebuild.

- **Serving DB** — the DB that answers all reads and takes live tip writes. The
  old DB until cutover; the new DB after. Named by the active pointer.

- **Building DB** — the new current-schema DB being bulk-synced from the
  validator in the background during a rebuild. Serves nothing until cutover.

- **Active pointer** — the durable record naming the serving DB's schema id. The
  single linearization point: flipping it *is* cutover, and on restart it names
  the authoritative DB.

- **Cutover (promotion)** — atomically flipping the active pointer from the old
  DB to the building DB once the building DB has caught up. New-schema
  capabilities become available at this instant.

- **Obsolete DB** — the old DB after cutover: no longer pointed to, scheduled for
  deletion once outstanding read handles drain.

- **Serving model during rebuild** — old DB stays live (live tip writes + all
  reads); whole-DB atomic cutover when the building DB reaches the old tip; no
  per-height routing. New-schema-only capabilities are unavailable until cutover.

- **Convergence** — the building DB bulk-syncs from genesis to the *current
  finalised tip* (reusing today's background-sync loop, re-reading the tip each
  batch). When it reaches that tip the active pointer is flipped atomically — **no
  write freeze**. This is safe because the finalised tip is quasi-static at
  catch-up (advances only when a block finalises, ~once per block interval) and
  the live write path is a single serialized loop that reads the active-pointer
  DB's tip and appends tip+1; flipping the atomic pointer between iterations
  keeps writes contiguous, and any single-block residual is closed by the normal
  live path on the now-active new DB. The new DB then serves as an ordinary
  index, no longer background-syncing.

- **Obsoletion policy** — delete-after-flip only: the obsoleted old DB is deleted
  right after a successful pointer flip (+ read-handle drain). There is **no**
  startup GC sweep. Consequence: a crash between flip and delete, or a stale
  build left by a double-upgrade, leaks disk but stays correctness-neutral —
  which holds *only* because (a) the active-pointer write is atomic + durable
  (single fsync'd record or atomic rename), so restart never reads a torn
  cutover, and (b) resume keys strictly on the current schema id, so a
  no-longer-current partial build is ignored, never mistaken for the target.

- **Resume** — on startup, a building DB resumes iff a *current-schema* dir
  exists with readable metadata and a recorded sync height; it continues from
  that height, trusting the existing fsync-checkpoint + idempotent-write
  crash-safety to absorb a torn tail. A partial of a different/no-longer-current
  schema, or one with unreadable metadata, is not a valid resume target — build
  fresh at the current schema. Applies to both the initial sync (no prior
  serving DB) and a rebuild (old serving DB present).

- **Degraded-but-serving** — a rebuild that cannot progress (validator down,
  repeated errors, insufficient disk) never takes serving down: the old DB is
  intact and keeps serving. The rebuild retries with backoff under a non-fatal
  degraded status + metric, and a ~2x-disk pre-flight check gates starting it
  (re-checked periodically if it fails). New-schema features stay unavailable
  until a rebuild eventually succeeds. Contrast: today a failed background
  migration escalates to `CriticalError`.

- **Cold start** — first run with no DB: the validator passthrough serves reads
  during the initial from-genesis sync so the service is available immediately,
  as today. When the initial sync completes the freshly built DB becomes the
  serving DB. The passthrough survives only for cold-start availability and the
  standalone `ephemeral` config mode — never during a rebuild (rebuilds serve
  from the live old DB).

- **Building wiring / cutover** — a dedicated rebuild task owns the building
  backend (a second persistent DB in its hash-named dir) and bulk-syncs it
  directly, *not* through the router's live write path; the live write path keeps
  appending to the old primary until cutover. At cutover the task atomically
  flips the durable active pointer, calls `Router::replace_primary(building)`,
  then deletes the returned old DB. The Router stays a minimal
  primary(+cold-start-ephemeral) dispatcher. `EphemeralMode::Full` (the
  migration primary-freeze) is removed; `EphemeralMode::ReadOnly` is kept for
  cold-start.

## Replaced / removed

The "migration tool" being retired: `MigrationManager`, the `Migration` trait,
`MigrationStep`, `MigrationType`, every per-version `migrate()` body, the
stepwise `get_migration` registry, and `MigrationStatus`. Retained and reused:
the Router (`replace_primary`), the ephemeral passthrough + `EphemeralMode::ReadOnly`,
`DbV1`, the capability model, `DbReader`, and the bulk-sync loop
(`write_blocks_to_height`) that now fills the building DB.
