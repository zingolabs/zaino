# Simplification plan: five features removed

Settled in a grilling session on 2026-09-23. Zaino's core product is zainod, a
lightwalletd server that builds its indexes from zebra over zebra's JSON-RPC
interface. Its secondary product is the non-standard indexes, such as
block-explorer indexes, that zainod also serves. This plan removes five
features whose complexity outweighs their value to either product.

## Decisions

1. **Zallet support ends.** "Zallet support" means the whole in-process
   embedding contract. zainod becomes the only consumer of every library
   crate (zingo-adrs `zaino/0021`).
2. **Post-ingest integrity checks go.** The per-row BLAKE2b checksums, the
   startup and background re-validation pass, the read-time validation gate,
   and the ingest-time merkle-root check are all deleted. The parent-hash and
   height continuity check stays, because it enforces the append-only rule of
   the finalised state, not data correctness. Its error is renamed as a
   continuity violation.
3. **Database migrations go.** The database records one schema identity, its
   schema hash. On a mismatch, zainod logs one line, deletes the index
   directory, and resyncs from zebra. `DbVersion`, `MigrationStatus`,
   `MigrationManager`, and the `db_version` config key are deleted.
4. **The versioned codec collapses.** `ZainoVersionedSerde` becomes one
   encode method and one decode method, with no version byte and no historical
   decoders. `zaino-encoding` folds into `zaino-chain-store-zainodb` as a
   private module.
5. **No index is served during sync.** zainod binds its gRPC and JSON-RPC
   listeners only after the finalised store reaches the finalised floor. The
   admin listener binds at startup and gains `/readyz`. The ephemeral backend,
   the router, `ChainStoreError::NotReady`, and the collapse of `NotReady` to
   "not found" are deleted.
6. **The read-state service goes, behind an evidence gate.** Its removal waits
   for elicbarbieri's benchmark showing that the JSON-RPC path matches or
   beats the read-state path. The evidence must address the ~323 versus
   ~174k blocks/s figures quoted in `zaino/0018`. The user asks for it.
7. **Three sequential pull requests** carry the work, as listed below.
8. **The index set is a build-time choice.** Each non-standard index is a
   cargo feature of zainod, and the lean set is the default. The capability
   bitmap is deleted (zingo-adrs `zaino/0022`, superseding `zaino/0018`).
9. **Node forwarding stays, and every index-miss fallback goes.** See the
   glossary entries in `CONTEXT.md`.
10. **PR 3 reuses #1495's decision set without the tombstone.** It is
    re-implemented on `dev`, not rebased from `bdec15aa`.
11. **The schema hash is computed.** A startup function hashes a canonical
    encoding of each persisted type, the table names and flags, and the
    enabled index features. `db_schema_v1.txt` and the hash constant are
    deleted. The goldens stay, and a golden failure means that the change
    forces every operator to rebuild.

## Pull request 1: store simplification

This PR needs no evidence and changes no public surface outside the store.

- Delete `entry.rs`'s `StoredEntryFixed` and `StoredEntryVar`; store bare
  encodings.
- Delete `v1/validation.rs` except the continuity check, which moves into
  the write path. Delete `validated_tip`, `validated_set`, the background
  validation loop, and `resolve_validated_hash_or_height`.
- Delete `verify_header_merkle_root` and `calculate_block_merkle_root`.
- Delete `store/migrations.rs`, the migration tests, `DbVersion`,
  `MigrationStatus`, `spawn_with_target_version`, and the `db_version` key.
- Collapse `ZainoVersionedSerde` and fold `zaino-encoding` into the store
  crate.
- Replace the schema constant with the computed schema hash, and add the
  wipe-and-rebuild on mismatch.
- Rewrite the header of `golden.rs` for the new failure protocol.
- Remove the historical-version writers from the `testing` feature.

## Pull request 2: serving after sync, and the end of embedding

This PR lands after zingo-adrs `zaino/0021` merges and the zallet maintainers
have been told.

- Delete the ephemeral backend, its tests, the router, and
  `ChainIndexConfig.ephemeral`.
- Delete the capability bitmap and `CapabilityRequest`, and gate the
  extension traits by cargo feature. This moved from PR 1: the router's
  masks route reads to the ephemeral backend during sync, so the bitmap
  cannot go before the router does.
- Delete `ChainStoreError::NotReady`, `StoreError::V1BackendUnavailable`, and
  the collapse at `reading.rs:43`.
- Gate the gRPC and JSON-RPC listeners on sync completion, and implement
  `/readyz` in `zainod/src/admin.rs`.
- Delete every index-miss fallback, starting with the `z_gettreestate`
  retry, and the "passthrough aware" notes and `chain_index_passthrough.mmd`.
- Merge `ChainIndexRpcExt` into `ChainIndex`, delete the `zaino-state`
  re-exports, narrow visibility, and mark every crate except zainod
  `publish = false`.
- Drop the embedder clause from the glossary entry for the preferred
  CryptoProvider.
- Delete the usage.md section "The ephemeral backend has two jobs".

## Pull request 3: the read-state service

This PR opens only after the evidence gate passes. Its ADR records the
evidence and supersedes the composite part of `zaino/0008`.

- Delete `zaino-source-zebra-readstate` and `zaino-source-zebra`.
  `ValidatorSource` in zaino-state becomes the composite over the JSON-RPC
  adapter.
- Delete the `backend` config key, `BackendType`, `ValidatorConnectionType`,
  `DirectConnectionConfig`, `StateServiceConfig`,
  `validator_grpc_listen_address`, and `zebra_db_path`, with no tombstone.
- Move `HashOrHeight` into zaino-primitives, pinned by golden vectors and a
  differential test against zebra's parser.
- Delete the dual-backend parity tests and keep each test's JSON-RPC leg.
- Fix the README crate index and delete the removed crates' usage guides.
