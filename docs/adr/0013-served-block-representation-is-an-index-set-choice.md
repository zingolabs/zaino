# The served block representation is a choice of index set

## Status

proposed — records an **open decision** that needs team input. This ADR frames the
question and the options; it does not yet select one.

Number provisional (see ADR-0012's status note on numbering).

## Context

Three block representations are in play:

- **Full block** — every transaction with its proofs and signatures. Expensive to
  fetch and to parse.
- **`PreIndexCompactBlock`** — a compact-deserialized block introduced by the
  modular sync engine (#1402). It keeps exactly the transparent inputs/outputs and
  shielded commitments that indexing and compact-serving need, and skips the heavy
  full-block parts. It is produced directly by Zebra's state service: a prototype
  `ReadRequest::CompactBlock` variant on the `nachog00/zebra` fork (patched in via
  `[patch.crates-io]`, pending upstream) returns the compact form straight from the
  state DB without parsing full transactions/proofs/signatures, consumed through the
  `GetPreIndexCompactBlock` source port. This is what takes bulk sync from
  ~323 blocks/s (header-only over JSON-RPC) to ~174k blocks/s (ReadState direct).
- **`CompactBlock`** — the proto form served to light wallets. It equals
  `PreIndexCompactBlock ⊕ ChainMetadata`, where `ChainMetadata` carries commitment
  **tree sizes**. Those sizes are *cumulative* state (they depend on all prior
  history), so they are present in no single source block; they are added when
  indexes are built or when a block is served.

Two in-flight PRs make *different, unstated* choices for the recent
(non-finalised) window's element:

- **#1418** stores the domain `CompactBlock` in the window — folding `ChainMetadata`
  at ingestion. This requires the running tree size to be **seeded at the freeze
  horizon** (a cumulative-state handoff, not just a block handoff).
- **#1440** stores a parsed `Block` and fetches tree roots **per-hash from the
  source on demand** — no fold, no seed, but a source round-trip per tree-root
  query and no serving-ready compact block in the window.

Neither the layout proposal nor either PR's ADR names this as a decision. It is,
however, a real fork with correctness (tree-size threading) and cost (sync speed
vs. per-query round-trips) consequences.

## Decision (framing, not yet selected)

**Frame the representation as a property of the *index set*, not a fixed
architectural choice.** The sync engine syncs declarative index-set definitions, so
*what a deployment stores locally is a configuration input*, and *what it cannot
answer locally is served by validator passthrough*. Passthrough here covers both
data Zaino never stores and data it has not synced *yet* (bootstrap/catch-up) — but
only for capabilities the validator can answer; a synthetic index (address history,
spend status) cannot be passed through and is simply not-serviceable until built.

Candidate index-set tiers:

- **Lean set** — `PreIndexCompactBlock` with `ChainMetadata` folded at index-build
  time. Cheapest to sync; sufficient for the serving layer to compose a full
  `CompactBlock` on demand. Very likely all the lightwalletd-compatible path needs.
- **Heavier set(s)** — full blocks (and/or additional aux indexes), synced only by
  deployments whose use cases need them (raw block, raw transaction, verbose block
  RPCs). The light path never pays this cost.

Under this framing the #1418-vs-#1440 disagreement largely dissolves: "store a
composed compact block" and "fetch from the validator on demand" are two points on
one spectrum — *what you pre-index vs. what you passthrough* — selected per
deployment by the index set. The serviceability manifest advertises, per
configuration, which reads are answerable.

## Open questions (must be resolved to select)

1. **Sufficiency of the lean set.** Is `PreIndexCompactBlock + ChainMetadata`
   provably enough to reconstruct a valid serving `CompactBlock` for *every* field
   the compact-serving path needs? Check field-by-field against the served schema;
   do not assume.
2. **Use-case → required-data map.** Enumerate which Zaino reads need more than the
   lean set. That list determines how many index-set tiers actually earn their keep.
3. **Cost trade.** Fetch-on-demand is cheap to sync, costly per query; pre-indexing
   the heavier set is the reverse. The winner depends on the deployment's read mix
   and should be measured, not guessed.

## Consequences

- The recent-window element is not one canonical type; it is chosen with the index
  set. Both #1418 and #1440 become configurations, and the seed-at-horizon
  mechanism (needed by the lean/`CompactBlock` path) becomes an explicit design
  item rather than an accident of which PR you read.
- Whichever tiers are adopted, the **freeze horizon must carry the cumulative tree
  size**, not just the block — a missed block is recoverable by refetch, a missing
  tree-size anchor is not.
- The `nachog00/zebra` `ReadRequest::CompactBlock` dependency is a prototype; a
  lean-set decision should track its upstreaming.
