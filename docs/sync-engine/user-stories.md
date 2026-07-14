# Sync Engine User Stories

Living document. Updated as development uncovers modifications or new stories.

Layered top-down by actor. Each layer is a consumer-provider boundary: the
story states what the consumer wants, and what the provider must never have
to know.

Last updated: 2026-07-09

---

## Level 0 — Application Builder (composes index sets)

The person building an application on top of Zaino. Picks which indexes to
run, which source to connect, and which storage to use.

> **US-0.1** As an app builder, I want to assemble an index set by picking
> indexes (`IndexSet::new().with::<HeadersIndex>().with::<AddrHistoryIndex>()...`),
> so that a light-wallet server, a block explorer, and an analytics job each
> run only the indexes they need — paying storage and sync cost only for those.

> **US-0.2** As an app builder, I want to plug in my own source (provisioner)
> and my own storage (backend) behind traits, so that the same index set runs
> against Zebra ReadState, JSON-RPC, or a test fixture, and persists to LMDB
> or in-memory, without touching index code.

> **US-0.3** As an app builder, I want registration to fail fast (cycle in
> deps, missing dependency, duplicate name) at build time, so misconfigured
> sets never start syncing.

---

## Level 1 — Index-Set Implementor (e.g., the Zaino Zcash set)

The person who owns *the* concrete index set for a blockchain. Wants to
define domain indexes and offload all orchestration to the engine.

> **US-1.1** As an index-set implementor, I want to declare *what* each index
> computes (scope, composition, deps, extract, schema) and get scheduling,
> parallelism, merging, atomic persistence, and crash-resume *for free*, so
> that I never write concurrency or transaction code.

> **US-1.2** As an index-set implementor, I want the set to commit as one unit
> per batch — all indexes' writes plus the watermark atomically — so that on
> restart the whole set resumes from one height with no per-index skew visible
> to readers.

> **US-1.3** As an index-set implementor, I want to define one set-wide block
> context and have each index receive its own projection of it
> (`ProvideContext`), so that adding an index that needs extra source data
> widens the context in one place, not in every index.

---

## Level 2 — Index Implementor (writes one index)

The person adding a single index to an existing set. Should never think
about scheduling, parallelism, transactions, or other indexes.

> **US-2.1** As an index implementor, I want to state my index's position on
> the Scope x Composition grid in the type system, so the compiler rejects an
> extract signature that reads data my scope doesn't grant (an L-scope index
> simply cannot ask for prior state or dep reads).

> **US-2.2** As an L-scope implementor, I want to write a pure per-block
> function `extract(ctx) -> Delta` and a merge algebra (or none, for Append),
> so the engine can parallelize across blocks without me knowing.

> **US-2.3** As an S-scope implementor, I want my prior accumulated state
> handed to me — loaded from the backend on resume, threaded across blocks and
> batches by the engine — so I never manage checkpointing myself.

> **US-2.4** As an X-scope implementor, I want a read handle restricted to
> *committed* state of my *declared* dependencies, so I can't accidentally
> read uncommitted or undeclared data. *(designed, not yet implemented)*

> **US-2.5** As an index implementor, I want to describe persistence as typed
> `(Key, Value)` entries (`Schema` + `Encode`), so serialization and schema
> versioning are separate from extraction logic.

---

## Level 3 — Intermediate Provider-Consumer Contracts

The boundaries between engine internals. Each story names both sides of the
contract.

> **US-3.1 (Engine <- Provisioner)** As the engine, I want an ordered stream
> of block contexts with backpressure from my bounded buffer, so a slow disk
> stalls fetching instead of ballooning memory; as the provisioner, I want to
> know nothing about indexes — I fill a buffer.

> **US-3.2 (Scheduler <- Engine)** As the engine loop, I want the scheduler
> to hand me only work that is safe to run right now (block available AND
> firing rules satisfied), so workers can execute blindly; as the scheduler,
> I want state transitions type-checked (`BatchHandle<FullyExtracted>` ->
> `<Merged>`), so out-of-order reporting can't compile.

> **US-3.3 (Engine <- Backend)** As the engine, I want a single atomic
> `commit(Vec<WriteOp>)` per batch plus a flush/durability boundary, and to
> know the writer topology (shared vs per-index), so I can order writes
> correctly for the store's concurrency model.

> **US-3.4 (Downstream index <- Upstream index)** As a downstream index, I
> want the upstream's commit of batch N to be my firing signal for batch N
> (Pipelined) or its full completion (Barrier), so cross-index reads are
> always over consistent committed state. *(Barrier stubbed)*

---

## Level 4 — Operator

The person deploying and monitoring Zaino.

> **US-4.1** As an operator, I want a persisted watermark and clean resume —
> kill the process at any point, restart, and it continues from the last
> committed batch with no manual intervention.

> **US-4.2** As an operator, I want sync progress observable (feature-gated
> tracing/metrics, non-default per privacy policy), so I can tell "syncing"
> from "stalled" from "done".

---

## Changelog

| Date | Change |
|------|--------|
| 2026-07-09 | Initial draft. 14 stories across 5 levels. |
