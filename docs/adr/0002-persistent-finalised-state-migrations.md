# Persistent finalised-state versioning and migrations

## Status

accepted

## Constraints (spec)

The *persistent finalised state* (the on-disk database backing `FinalisedState`,
as opposed to the *ephemeral finalised state* that serves from the backing
validator) evolves under a versioned schema. These constraints are the binding
contract for that evolution. A migration that violates any of them is a bug,
regardless of whether it appears to work in the common case.

**C1 — One authoritative version fingerprint.** `DbMetadata::version`, a single
`DbVersion {major, minor, patch}` persisted inside each database directory, is
the *sole* source of truth for the version of the data in that directory. There
is no separate set of "applied migrations".

**C2 — Versions form a forward-only graph; migrations are its edges.** Each
migration maps one exact `DbVersion` to the next. Reaching a target version is
finding a path through that graph from the current fingerprint and running its
edges in order. A missing path is a hard error, never an unsafe fallback.
Downgrades are not edges (see C7).

**C3 — Every migration is exactly one of three types**, distinguished by the
version boundary it crosses, and the type fixes both its meaning and its
lifecycle:

- **patch** — code-only; the on-disk schema is byte-for-byte unchanged. Only the
  version marker advances. No body.
- **minor** — a schema/data change **within the same major version**. Always
  engineer-implemented (inherently bespoke): the rebuild may read from data
  already on disk and/or refetch from the validator, at the engineer's
  discretion.
- **major** — a switch **between major versions** (a new directory/backend,
  possibly a different storage engine). By default it builds the new major from
  the backing validator from scratch, which produces the *newest available*
  version of that major — so the default path lands directly on that newest
  version and does **not** separately run the new major's intermediate
  minor/patch migrations. The body is overridable: an engineer may implement a
  faster strategy (e.g. building the new major from the current primary's data
  instead of refetching from the validator).

  Rule of thumb: code-only, no schema change → *patch*; a change within a major →
  *minor*; a change of major → *major*.

**C4 — Resumable.** A migration interrupted at any point (crash, kill, power loss)
resumes correctly on the next startup from durable progress, and never restarts in
a way that destroys correct work or double-applies a non-idempotent step.

**C5 — Single completion gate.** The persisted `DbMetadata::version` is the only
signal that a migration finished. It is made durable (fsync) **before** any
irreversible cleanup (e.g. deleting an old major). A crash before the gate leaves
the version unchanged, so the migration is re-selected and resumes; a crash after
it leaves only dead, reclaimable state.

**C6 — No corruption, no partial promotion.**
- A **minor** migration mutates the one primary in place, is idempotent on resume,
  and does not advance `DbMetadata::version` until the rebuild is complete and
  durable.
- A **major** migration never mutates the existing primary. The new backend is
  exposed only by an atomic primary swap, and only after it is fully built and
  fsynced. A crash at any point leaves the existing primary intact and
  authoritative; a half-built database is never served.

**C7 — Authoritative selection from disk alone; downgrades are selection.** On
startup, the live primary is chosen from on-disk state. A directory is
*Authoritative* (a fully-synced DB — including one that crashed mid-patch or
mid-minor, whose data is complete at the old version) or *IncompleteBuild* (a
major build target still being built; its metadata is stamped in-progress before
any data is written and cleared only at the completion gate, or it is
missing/partial). An IncompleteBuild directory is never opened as primary.
Selecting an older major that still exists on disk is *selection* of an
Authoritative directory, not a reverse migration.

**C8 — Major directories may coexist; retention governs deletion.**
`DatabaseConfig::old_db_retention` (default **Keep**) decides whether the old
major's directory survives a successful promotion. **Keep** enables instant
switch-back (no rebuild) at the cost of disk; **Delete** reclaims disk and makes a
later switch-back a fresh build. Deletion runs only after the completion gate is
durable.

## Implementation

The model lives under
`packages/zaino-state/src/chain_index/finalised_state/`.

**Version registry and planner** (`migrations.rs`). The supported migrations are a
single authored list passed to the `migrations! { … }` macro, which generates the
`MigrationStep` enum, its static dispatch (no `dyn`), and the planner's `(from,
to)` edge list — satisfying C2 from one place. `plan_migrations(current, target)`
builds the adjacency from those edges and computes a deterministic path, erroring
when none exists. `MigrationManager::migrate()` computes the plan once and runs
the steps in order, advancing the working version to each step's `TO_VERSION`.

**The `Migration<T>` trait** carries `CURRENT_VERSION` / `TO_VERSION` (C1's
fingerprints as graph nodes), `from_version()` / `to_version()`, and
`migration_type()` (default `Patch`). Its default `migrate()` implements the
generic behaviour for the non-bespoke types: a metadata-only advance for **patch**,
and the build-and-promote helper for **major** (keyed off `TO_VERSION.major`).
Default impls keep patch and major bodies empty (C3).

A **minor** migration must override `migrate()`. A true compile-time check is not
expressible here, because the migration type is a runtime discriminant
(`migration_type()`) and the default body cannot know at compile time whether it
was overridden. The default `migrate()` therefore guards against a `Minor` that
reached it (i.e. did not override) by failing immediately, before any work, with a
clear error naming the migration. This is backed by a registry test asserting that
every registered `Minor` overrides the default.

The **default major** `migrate()` builds the new major from scratch, which yields
the newest version of that major. Its `TO_VERSION` is therefore
`latest_version_for_major(new_major)`, and the from-scratch build subsumes — does
not separately run — the new major's intermediate minor/patch migrations.

**Per-type lifecycle is owned by `MigrationManager`**, so bodies stay minimal:
- *patch* runs against the primary and advances `DbMetadata`; no ephemeral.
- *minor* installs a full-mode ephemeral reference
  (`Router::init_or_take_ephemeral(EphemeralMode::Full)`) so reads are served from
  the validator while the primary is rebuilt (from on-disk data and/or a validator
  refetch), runs the body, advances `DbMetadata`, and releases the reference (C6
  minor).
- *major* runs the build-and-promote helper (below), which manages its own brief
  ephemeral freeze.

**Build-and-promote helper** (`migrations.rs`, the default major path), realising
C4–C6 for majors:
1. `FinalisedSource::spawn_major(target_major, cfg)` spawns the target backend in
   its own directory and, as its first durable action, stamps the directory's
   `DbMetadata` with an in-progress status so a crash classifies it as
   *IncompleteBuild* (C7).
2. It syncs the target backend to tip from the `BlockchainSource` while the
   existing primary keeps serving read+write; the build resumes from the target
   backend's own append-only tip (C4), and the existing primary is never mutated
   (C6).
3. A brief ephemeral freeze does the final catch-up, then the **completion gate**:
   the target backend's `DbMetadata` is set to the target version — the newest
   version of the new major (`latest_version_for_major`) — and fsynced (C5). Only
   now is the directory *Authoritative*.
4. `Router::replace_primary` atomically swaps the new backend in as primary, and
   the ephemeral reference is released.
5. The old primary is shut down and its directory is kept or deleted per
   `old_db_retention` (C8).

**Startup selection** (`finalised_state.rs`). `detect_majors_on_disk(cfg)`
enumerates the legacy v0 layout and each versioned major directory
(`finalised_source::VERSION_DIRS`) and reads each one's `DbMetadata`, classifying
it *Authoritative* or *IncompleteBuild* (C7). `latest_version_for_major(major)`
maps the configured major to its latest `DbVersion` (`major 1 => DB_VERSION_V1`).
`FinalisedState::spawn` then, for the configured target major:
1. target directory *Authoritative* → open it; the planner resumes any in-flight
   patch/minor and migrates forward to that major's latest;
2. target directory absent or *IncompleteBuild* → open the best *Authoritative
   older* major to keep serving, and run/resume the major migration to the target
   (the half-built target is never served);
3. no *Authoritative* directory → build the target major fresh from genesis.

A crashed major migration needs no special path: the older directory is
Authoritative, the configured target is the new major, so the planner re-selects
the same major edge and the helper resumes the partial build (C2 + C4).

**Configuration.** `DatabaseConfig::old_db_retention: OldDbRetention { Keep,
Delete }` (default `Keep`, `packages/zaino-common/src/config/storage.rs`) is
threaded through `ChainIndexConfig` (`packages/zaino-state/src/config.rs`) and read
by the major helper (C8).

## Engineer / dev guide

Adding a supported version is "write a small `impl` and add one line to the
registry". Choose the type by C3's rule of thumb.

**Patch** — schema unchanged; only the version marker advances:

```rust
struct Migration1_2_0To1_2_1;
impl<T: BlockchainSource> Migration<T> for Migration1_2_0To1_2_1 {
    const CURRENT_VERSION: DbVersion = DbVersion::new(1, 2, 0);
    const TO_VERSION:      DbVersion = DbVersion::new(1, 2, 1);
    // default migration_type() = Patch, default migrate() = metadata-only
}
```

**Major** — switch to a new major; default builds from the validator to that
major's newest version:

```rust
struct Migration1_2_1To2_0_0;
impl<T: BlockchainSource> Migration<T> for Migration1_2_1To2_0_0 {
    const CURRENT_VERSION: DbVersion = DbVersion::new(1, 2, 1);
    // TO_VERSION is the NEWEST version of the new major: the default from-scratch
    // build lands there directly, skipping that major's minor/patch migrations.
    const TO_VERSION:      DbVersion = DbVersion::new(2, 0, 0);
    fn migration_type(&self) -> MigrationType { MigrationType::Major }
    // default migrate() = build-and-promote from the validator, keyed off
    // TO_VERSION.major. Override migrate() for a bespoke major — e.g. building
    // the new major from the current primary's data instead of refetching.
}
```

**Minor** — schema/data change within the same major; inherently bespoke:

```rust
struct Migration1_1_0To1_2_0;
impl<T: BlockchainSource> Migration<T> for Migration1_1_0To1_2_0 {
    const CURRENT_VERSION: DbVersion = DbVersion::new(1, 1, 0);
    const TO_VERSION:      DbVersion = DbVersion::new(1, 2, 0);
    fn migration_type(&self) -> MigrationType { MigrationType::Minor }
    async fn migrate(&self, /* router, cfg, source */) -> Result<(), FinalisedStateError> {
        // Rebuild the affected table(s) from data already on disk and/or by
        // refetching from the validator. MUST be resumable and idempotent on
        // resume (C4), and MUST NOT advance DbMetadata::version until the rebuild
        // is complete and durable (C6).
    }
}
```

Then register the type so the planner gains its edge:

```rust
migrations! {
    Migration1_0_0To1_1_0,
    Migration1_1_0To1_2_0,
    Migration1_2_0To1_2_1,
    // Migration1_2_1To2_0_0,  // when DbV2 lands
}
```

**Also update the version constants** in the versions module — the registry edge
and the latest-version source of truth must agree:

- *patch / minor*: bump the affected major's latest-version constant (e.g.
  `DB_VERSION_V1`) to the new `TO_VERSION`, so fresh databases and
  `latest_version_for_major` target it.
- *major*: add the new major's latest-version constant, add its
  `latest_version_for_major` arm (so the default major build targets the newest
  version — see above), and append its directory to `VERSION_DIRS`.

Adding a new **major backend** (e.g. DbV2): implement the backend, add its
`FinalisedSource` variant and `FinalisedSource::spawn_major` arm, append its
directory to `VERSION_DIRS`, add its latest-version constant and
`latest_version_for_major` arm, and add its (major) migration line to the
registry. Its build-and-promote behaviour is inherited.

**Review checklist** for any change under `finalised_state/` that adds or alters a
migration: confirm C4 (resumable), C5 (completion gate ordering), and C6 (no
partial promotion / in-place idempotence) by reading the body, not by assuming
the common path. Minor migrations especially must not advance the version marker
before the rebuild is durable.
