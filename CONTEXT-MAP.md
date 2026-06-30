# Context Map

zaino is a single Cargo workspace spanning several bounded contexts. Each context
keeps its own `CONTEXT.md` glossary; this map indexes them and records how they
relate. System-wide decisions live in `docs/adr/`.

## Contexts

- [Shielded pools](./docs/contexts/shielded-pools/CONTEXT.md) — Zcash value pools
  (Sapling, Orchard, Ironwood) and the per-pool action / commitment-tree
  vocabulary the indexer reads from transactions and serves to wallets.
- [Chain index](./packages/zaino-state/src/chain_index/CONTEXT.md) — the
  non-finalized / finalized block-index state model (NFS, finalized state, seam,
  eviction).
- [Live tests](./live-tests/CONTEXT.md) — the live-validator test-suite taxonomy
  (the `e2e` and `clientless` partitions).

## Relationships

- **Shielded pools → Chain index**: the chain index extracts each pool's actions
  and outputs from blocks into compact blocks; it consumes the pool vocabulary
  (e.g. Ironwood action) defined in the Shielded pools context.
- **Shielded pools → wire/proto**: the compact pool types in `zaino-proto`
  (`CompactOrchardAction`, reused for Ironwood) realize this vocabulary on the
  wire.
- **Live tests** stands alone — it names a testing taxonomy, not a domain
  concept, and references the other contexts only by exercising them.
