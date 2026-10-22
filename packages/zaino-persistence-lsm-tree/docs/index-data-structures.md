# Map segment filters

Why every map segment carries a sharded BinaryFuse8 filter, and what it costs. How maps are laid
out and merged is in [durability.md](./durability.md) §4.

## Two read patterns

A map namespace is read one of two ways, and its `Tuning::scope` says which:

- **Probed** (scope 0): point lookups by a uniformly distributed key, never scanned, for example
  `txid ‖ vout` → spend. The common case is often a miss in every segment (an unspent output is
  absent from a set of spends), so each segment's filter covers the whole key and turns most of
  those misses into no read at all.
- **Scoped** (scope `n`): range scans that share their first `n` key bytes, for example every row
  of one address under `address ‖ height ‖ outpoint`. The filter covers only those `n` bytes, so a
  range within one scope asks each segment's filter whether the scope is there at all and skips
  every segment that never saw it. When most scopes are active for a short stretch, this turns a
  seek in every segment into a seek in the few that hold the scope. Put the uniform bytes first:
  the filter shards on a key's first 8 bytes, which must be uniform.

Each segment records how many key bytes its filter covers, so changing a map's scope never
misreads old segments: the open rewrites them under the new scope before serving.

Readers read and check each segment's filter when they map it, so a probe never faults in a cold
filter page, and the filters stay resident as the page cache allows.

## Sizing

Segments never change after they are built, which is what a static filter is for (RocksDB's
per-SST filters are the same idea), so we use a sharded BinaryFuse8. Measured with xorf 0.13,
BinaryFuse8 and BinaryFuse16 both build at about 100 ns per key and probe in about 17 ns, at 9 and
18 bits per key and false-positive rates of 2⁻⁸ and 2⁻¹⁶. With 8-bit fingerprints a miss wastes
about 2⁻⁸ block reads per live segment (0.004; a higher fanout = more live segments per probe).
The filters only stay cheap while resident: for a set of about 190M rows (every transparent output
on mainnet, Sep 2026) they cost about 200 MB against 400 MB, so the smaller one wins. Build
throughput does not separate them: it is about 400 ns per row over a row's life, across roughly
`log_fanout` merges.

Each shard holds at most 2²⁰ keys, chosen by the top bits of the key's first 8 bytes (zero-padded
when the key is shorter), which must be uniform. That bounds build scratch at about 26 MiB whatever
the segment's size, and since shards close in key order, a merge builds them as it streams.
