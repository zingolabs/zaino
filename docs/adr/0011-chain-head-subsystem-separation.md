# The non-finalised chain head is a self-synchronising subsystem

## Status

accepted

## Context and decision

The non-finalised state was a module inside `zaino-state`
(`chain_index/non_finalised_state.rs`, 986 lines). It held the recent block
graph and the reorg handling, which is where it belonged. Everything about
*how* it was reached was wrong, and four consequences followed.

**It could not advance without the finalised state.** It read
`FinalisedState::db_height()` to pick its anchor, and `DbReader` to resolve
blocks below its window. A deployment with no database — the ephemeral mode
added for exactly this reason — took the fallback arm of every one of those
reads. The dependency existed in the types without ever being needed by the
logic.

**It could not advance without being told to.** It had no task. `ChainIndex`'s
sync worker called `sync(finalized_db, chain_height)`, so the two layers
advanced in lockstep at whichever rate suited the slower one, and the freshness
of the chain tip was decided by the database's write throughput. Any consumer
holding the module could also drive it, so "how far along is the head" was a
question about the caller rather than about the chain.

**Publication was not atomic.** `sync` called `update()` every `depth` blocks
mid-catch-up and re-anchored by storing into the shared cell directly, so a
reader could observe a partially-filled window or a half-applied reorg. The
compare-and-swap this was built on defended against a second writer that did
not exist.

**Its boundary was `IndexedBlock`**, the persistence type. A representation
chosen for what a database row needs was the currency of an in-memory graph
that has no database.

We decide:

1. **The chain head is its own subsystem, in two crates.** `zaino-chain-head`
   is vocabulary and ports — no runtime, no data structures.
   `zaino-chain-head-service` is the runtime. This is the same
   `<domain>` / `<domain>-service` split the rest of Zaino's layering uses, and
   it is what lets a consumer name what the chain head answers without
   depending on the machinery that answers it.

2. **It owns one writer task and synchronises itself.** There is no `sync`,
   `sync_to_height`, `reconcile` or `advance` on any public port, at any
   visibility. A consumer that could drive it could sequence it against
   something else, which reintroduces precisely the coupling this separation
   removes. The read handle, `ChainHeadSubscriber`, cannot start, stop or step
   it — that is a property of the types, not a convention.

3. **It never reads the finalised state.** The anchor is `tip - max_depth`,
   computed from the validator's tip. This is the arm the old code already took
   whenever the database lagged; removing the other arm removed the dependency
   without changing the behaviour anyone was getting.

4. **Snapshots are values, published atomically.** A candidate is built to
   completion and installed with one store. Every published snapshot described
   the chain at a single instant, so a reader capturing one and asking it
   several questions gets consistent answers, and can hold it across a reorg.

5. **`ChainHeadSnapshot` is a trait**, not a struct. The graph's representation
   — today `MapBackedSnapshot`, cloned per publish — is the runtime's business.
   Replacing it with persistent structures that share unchanged subtrees between
   publishes must be invisible to consumers, and with the type abstract it is.

6. **Reads live on the snapshot; the handle produces snapshots.** The handle
   has `current()` and `subscribe_updates()` and nothing else. Restating each
   query on the handle would define every capability twice and would let a
   caller ask two questions of two different views by accident. This matches the
   layering the chain view above it uses, so snapshot-pinned reads are
   structural at both levels.

7. **The driven port names only what the chain head asks.**
   `ChainHeadBlockSource` is a bound alias over five `zaino-source` ports with a
   blanket impl: `GetChainTip`, `GetBlock`, `GetBlockByHash`,
   `GetCommitmentTreeRoots`, `SubscribeBlocks`. Not `GetChainTips` — see below.

## `getchaintips` is not a question the chain head asks

The port originally carried `GetChainTips`, on the assumption that competing
branches are discovered by asking the validator to enumerate tips. The original
implementation never did this. It reached non-canonical blocks two ways:

- **Reorg fallout.** When the block after the tip has a parent the head does not
  hold, the reorg walk fetches ancestors by hash until it reaches a retained
  block. Separately, extending the chain overwrites the height index at each
  height while leaving the blocks themselves in place, so yesterday's best chain
  becomes today's competing branch and stays for the width of the window.
- **A block-carrying listener that was dead code.** Every production source
  returned `Ok(None)`, so the handler early-returned and the fifty-odd lines
  behind it were unreachable.

So the chain head only ever knew branches it personally lived through, and it
still does. Discovering a fork that existed in parallel but was never its own
best chain is a **new capability**, not part of this separation. Keeping
`GetChainTips` in the bound would oblige every source to answer a question
nothing poses, and would misdescribe what the subsystem needs.

Recorded for whoever builds that capability: **`zebra-rpc` does not implement
`getchaintips`** — zero occurrences in the crate. It will need a ReadState-backed
answer before it can work against zebrad at all. That cost a full live-suite run
to discover.

## Freeze handoff

A block that falls below the consensus seam — `tip - max_depth`, past which no
reorg can reach it — is emitted on a broadcast channel as a whole
`ChainHeadBlock`, tree roots included, so a chain store can ingest it without
fetching it again. This is the one capability here with no equivalent in the
original, added deliberately because the alternative is designing the port later
against a consumer that already exists and constrains it.

The stream is **best-effort by design**, and this is the part to understand
before relying on it. It is a `broadcast`: the chain head follows the tip and
must never stall on a slow consumer, so a consumer that falls behind gets
`RecvError::Lagged(n)` and learns exactly how many it missed. A chain head
re-anchoring after an outage moves its floor discontinuously and never emits the
skipped blocks at all. Neither is an error condition. The store's own build from
source is the authority; this only spares it the fetch in steady state.

## Consequences

**Startup is fallible where it was not.** A chain head has no persistent state
and no second data source, so one that cannot reach its validator has nothing to
offer. It anchors before its constructor returns or fails, and `ChainIndex::new`
fails with it — where the old code retried in the background indefinitely.
Transient failures are still absorbed by the retry ladder; sustained
unavailability at startup is now reported instead of hidden. In exchange,
`current()` is total for the rest of the process's life, and the "still
syncing" case disappears from every read path.

**The two layers now advance independently.** The finalised state's sync worker
no longer drives the head, so a slow database no longer holds the tip back.
Under ephemeral operation the head serves tip queries immediately while the
database is still building, which the live suite covers.

**`zaino-state` keeps a temporary conversion.** `chain_index/chain_head.rs`
converts `ChainHeadBlock` back into `IndexedBlock`, because the query paths in
that crate still read the persistence type. It exists so the subsystem could be
extracted without rewriting every query path in the same change, and it
disappears when `ChainIndex` becomes the chain view layer. Its round-trip test
compares it against the finalised state's own conversion field by field, so the
two cannot drift while both exist.

**The retained graph answers `getchaintips`.** With no validator fallback — the
same answers the old derivation produced from the same graph.

**The chain head is now the mempool's notion of "which chain".** The mempool
subsystem's coherence layer freezes and thaws against a non-finalised-state
epoch (ADR-0010); with the head extracted, that epoch is published by the chain
head, read
through the same subscriber the rest of `ChainIndex` serves snapshots from.
Two consequences worth naming:

- The epoch is read *from a snapshot*, not from the handle. A coherence check
  asks whether a transaction set matches the chain the caller is being served,
  and the caller is being served a captured view; comparing against the handle
  would compare against whatever has been published since.
- The sync loop no longer relays a publication signal. It does not drive the
  head and so does not know when the head publishes; the head's own epoch feed
  is both more accurate and one fewer thing to keep in step. `zaino-state`
  bridges that feed to the port's unit-typed wake, which is the one piece of
  glue this arrangement costs.

Neither subsystem names the other's types: the translation lives in
`chain_index/chain_head.rs`, which is the only place that knows both.
