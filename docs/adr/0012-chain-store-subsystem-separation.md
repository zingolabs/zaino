# The finalised state is a subsystem behind ports, and its database is one implementation of them

## Status

accepted

## Context and decision

The finalised state was a directory inside `zaino-state`
(`chain_index/finalised_state/**`, ~16.8k lines, plus ~4.2k lines of on-disk
types under `chain_index/types/db/**`). What it did — hold the blocks below the
reorg seam and the indexes built over them — belonged there. Four things about
*how* it was reached did not.

**There was no way to name the finalised state without naming its database.**
Every consumer bound on `DbReader`, an LMDB-shaped trait whose methods return
LMDB-shaped types. Substituting a different store meant replacing a crate that
everything else spelled out by name, so "a second backend" was not a matter of
satisfying an interface; there was no interface.

**Its read surface was ~40 methods, of which 12 had a caller outside the
directory.** The rest were internal assembly helpers that had become public
because the module boundary was a directory rather than a crate. A reviewer
could not tell the contract from the scaffolding.

**`tonic::Status` was baked into the storage layer.** `CompactBlockStream` was
`ChannelStream<CompactBlock>` with a gRPC status as its error type, so an LMDB
cursor desync was phrased as a transport-level error some two thousand lines
below the transport.

**A range of blocks was N sequential read transactions.** `get_block_range`
opened one `begin_ro_txn` per height and sent one item per block through a
bounded channel, and the `StoredBlock` path carried a standing
`TODO: Add separate range fetch method!`. Reading a large range paid per-block
transaction setup and ~1000× more channel sends than the work required.

We decide:

1. **The finalised state is its own subsystem, split domain from adapter.**
   `zaino-chain-store` is vocabulary and ports — no runtime, no storage, no
   `zebra-chain`, no `tonic`. `zaino-chain-store-zainodb` is the LMDB
   implementation and the on-disk types it is built from. This is the same
   `<domain>` / `<adapter>` split ADR-0008 established for validator access and
   ADR-0010 for the chain head.

2. **The on-disk types are adapter-private, and the orphan rule enforces it.**
   `Persistent*` shapes live with their encoders and their golden vectors in the
   backend crate. They are lossy by design — a stored transparent output holds a
   20-byte address key, not the locking script — and that lossiness is why the
   domain has its own `StoredTxOut` and `StoredAddress` rather than reusing
   `TransparentOutput` and `TransparentAddress`.

3. **Capability is a set of traits, plus a runtime bitset where it must be.**
   Only `ChainStoreReader` is universal. Compact blocks, transaction positions,
   spent outputs, the txout set and address history are each their own trait, so
   a consumer's bound names exactly what it uses. `StoreCapabilities` remains a
   runtime value because a store on an older schema genuinely lacks an index
   until it has migrated — that is a fact about a database, not about a type.

4. **The chunk is the block-read primitive.** `blocks_chunk` / `compact_chunk`
   return a `Vec` from one read transaction; `blocks_stream` /
   `compact_stream` return an opaque `impl Stream` of chunks. There is
   deliberately no single-block method: a single block is a chunk of one, and
   naming a point read would invite the per-height transaction loop back. The
   error type is `ChainStoreError`; `zaino-serve` maps it to a status.

5. **The pool filter is pushed into the compact read, not applied after it.**
   `CompactBlockRead` is separate from `StoredBlockRead` for this reason alone:
   the filter selects which cursors open and which row families decode, so
   deriving compact blocks from stored ones would make a sapling-only wallet pay
   to decode orchard, ironwood and the commitment-tree rows on every block.

6. **The store owns its source.** `ChainStoreIngest::build_to` takes a target
   height and no source argument, so a consumer cannot repoint a running store
   mid-flight. There is no `sync(source, height)` on any port.

7. **The UTXO-set commitment is defined in the domain crate, and computed
   there.** `hash_serialized`, and the `bytes_serialized` beside it, are numbers
   two deployments can compare. Three things therefore belong to no single
   backend: the domain tag `b"ZcashTxOutSet___"`, the canonical entry's field
   order and widths, and the 65-byte entry length that `bytes_serialized` is a
   multiple of. All three live in `zaino-chain-store::txout_set`, along with the
   per-output fold that applies them.

   This is not a disk format. The canonical entry is a hash preimage — built,
   hashed, dropped, never written — and the crate deliberately defines no
   storage encoding for the accumulator. `zaino-chain-store-zainodb` keeps its
   own persisted row and its own `ZainoVersionedSerde` for it, and delegates the
   arithmetic. What the contract *does* require of any adapter is the ability to
   produce five values per unspent output: txid, output index, value, a 20-byte
   address hash and a one-byte script tag. That last pair is a real constraint
   worth naming — the commitment bakes in Zaino's lossy transparent encoding, so
   an adapter storing whole `script_pubkey`s must still reduce each to that pair
   to compute it.

   The reason this is a decision and not an implementation detail: two adapters
   that disagree here do **not** fail. Each is internally consistent, no read
   notices, and nothing errors — they simply report different numbers for the
   same chain. There is no runtime check to add, so the only available
   enforcement is a single definition, which is what this is. The digest and the
   fold previously existed in two copies that agreed only because one was
   written from the other.

8. **The ephemeral backend moves verbatim, both roles intact.** It is the
   passthrough for deployments with no database *and* the read shim
   (`init_or_take_ephemeral`) that answers while a long build or migration is in
   progress. Losing the second role means a store 100k blocks behind returns
   `None` for every read.

## What deliberately did not change

The integrity machinery moved byte-for-byte, because it is load-bearing rather
than belt-and-braces. The environment is opened `MDB_NOSYNC`, and the documented
consequence is that a hard eviction on networked or overlay storage can leave
torn pages. Per-row BLAKE2b-256 checksums over `encoded_key ‖ encoded_value` are
what turn that into `"checksum mismatch"` with a hex dump instead of a wrong
answer, and the **key binding** is what defeats a relocation or splice. The
version-searching `verify` is what makes mixed-version rows in one table safe.
The background validator, the startup spent-table sweep with byte-level
diagnostics, and the migration completion gate — advance the version durably
*before* deleting the progress key — all move unchanged.

Because it moved unchanged, it had to be proved unchanged. Golden hex vectors
for every `ZainoVersionedSerde` implementation were checked in **before** the
first line moved, and pass identically after. A build-time assertion that
`blake2b(DB_SCHEMA_V1_TEXT)` equals the `DB_SCHEMA_V1_HASH` literal closes the
one gap in that scheme, where the drift detector could itself drift.

## Two disagreements between indexes, now stated rather than implied

The address index and the txout-set accumulator do not agree about which
outputs exist, and this is intentional. `build_transaction_output_histories`
keys **every** output, storing a non-standard script under its first ≤20 bytes
tagged `0xFF`; the accumulator excludes those via `is_unspendable`, mirroring
zcashd's `IsUnspendable()`. Each port now states which semantics it exposes.
Recorded as a live hazard: the `len == 21` arm of that encoding reinterprets any
21-byte script as `[type_byte ‖ hash]` and can land under a P2PKH or P2SH key.
Fixing it is an on-disk semantic change and needs a migration.

The store also **cannot answer maturity questions**. Coinbase is special-cased
on inputs — null prevouts are filtered — and there is no coinbase flag on any
stored output. No port pretends otherwise.

## Consequences

**A second store is now a matter of implementing traits.** Two mechanical
obstacles remain in the adapter and are recorded here for whoever takes them on:
`DbWrite::write_blocks_to_height` is generic per method and the backend surface
is RPITIT throughout, so neither is `dyn`-safe; and `FinalisedSource<T>` is a
closed `V1 | Ephemeral` enum matched exhaustively in three places. That enum is
the real second-adapter seam, and it is not a trait yet.

**`zaino-state` reads the store through a bridge, not through LMDB.**
`chain_index/chain_store.rs` carries `WithChainStoreSource` beside the chain
head's equivalent. `ChainIndex` still exists and still passes its suites, which
is what makes this change revertable.

**`StoreCapabilities` is interim, and says so in its own documentation.** It is
the finalised state's internal routing model surfaced so `ChainIndex` keeps
working. It is storage-shaped — one bit per backend trait — where the layer
above needs a domain-shaped answer to "what is answerable to height H". The
chain view replaces its consumer; nothing new should be built on it.

**One tonic remnant survives in the adapter.** The domain ports are
`tonic`-free, but `ChannelStream` and the six serving stream aliases
(`RawTransactionStream`, `CompactBlockStream`, …) that `zaino-state` re-exports
still carry `tonic::Status`, and so `zaino-chain-store-zainodb` still depends on
`tonic`. Those aliases belong to the serving layer and leave with it when the
chain view lands; the layering violation is confined to a type alias rather than
threaded through the read paths.

**The finalised-state test suite moved with the code it tests.** Unit,
migration, v1 and ephemeral suites run in the backend crate against a local fake
validator. The mockchain and proptest suites stay in `zaino-state`, driven
through the new surface, and read the vector chain from the backend crate's
`testing` feature so that both sides compare against one oracle rather than two
copies of one.
