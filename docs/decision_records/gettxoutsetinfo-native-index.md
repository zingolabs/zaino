# Native `gettxoutsetinfo` Indexing Decision

## Status

Accepted.

## Context

Phase 1 of `gettxoutsetinfo` is implemented as a passthrough RPC to the backing validator.
Phase 2 will add native Zaino support.

The zcashd `gettxoutsetinfo` implementation computes statistics from its transparent UTXO set.
The response includes `bytes_serialized` and `hash_serialized`, which are derived from zcashd's
serialized coin records. The serialized hash includes the full transparent `CTxOut`, including the
full original `scriptPubKey`.

Zaino's current finalised-state transparent output representation, `TxOutCompact`, does not retain
the full script for every output. It stores the value, a 20-byte script/address hash, and a script
type. For non-standard scripts, this is lossy and cannot reproduce zcashd's serialized hash exactly.

## Decision

Use a validator-assisted migration for native `gettxoutsetinfo` support.

The migration will add native txoutset tables and backfill them by fetching historical full blocks
from the backing validator. This does not rebuild all existing Zaino finalised-state indices, but it
does perform a one-time chain scan for the new txoutset index so Zaino can preserve the full
transparent output scripts needed for zcashd-compatible `hash_serialized` and `bytes_serialized`.

## Consequences

- Existing Zaino instances can migrate without rebuilding unrelated finalised-state tables.
- The migration requires access to a backing validator that can serve historical full blocks.
- Migration progress must be resumable, using txoutset metadata such as `built_to_height`.
- Native `gettxoutsetinfo` should not advertise exact support until the txoutset backfill has
  completed.
- A no-refetch migration from existing compact transparent data is rejected because it cannot
  guarantee zcashd-compatible `hash_serialized` for existing databases.

## Implementation Outline

- Add txoutset LMDB tables for unspent outpoints, per-transaction unspent output counts, and
  txoutset metadata.
- Store full transparent output scripts in the txoutset UTXO records.
- Add a migration from the current DB version to the new schema version that fetches full blocks
  from the validator and applies transparent spends and outputs in height order.
- Make the migration resumable from `txoutset_meta.built_to_height`.
- Add sanity tests that verify migrated UTXOs, spent-output removal, transaction counts, total
  transparent amount, and migration metadata.
- Add compatibility tests against zcashd for all returned fields, including `bytes_serialized` and
  `hash_serialized`.
