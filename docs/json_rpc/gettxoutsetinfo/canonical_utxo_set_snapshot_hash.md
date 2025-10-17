    ZI-ng-P: 0
    Title: ZAINO-UTXOSET-01: Canonical UTXO Set Snapshot Hash (v1)
    Owners: A Nym <somenym@zingolabs.org>
            Za Wil <zancas@zingolabs.org>
    Status: Draft
    Category: Lightclients
    Created: 2025-10-16
    License: MIT

## Terminology

- The key words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are to be interpreted as described in BCP 14 [^BCP14] when, and only when, they appear in all capitals..
- Integers are encoded **little-endian** unless otherwise stated.
- “CompactSize” refers to the [Bitcoin Specified](https://en.bitcoin.it/wiki/Protocol_documentation#Variable_length_integer) [Zcash Implementation](https://docs.rs/zcash_encoding/0.3.0/zcash_encoding/struct.CompactSize.html) of variable-length integer format.
- `BLAKE3` denotes the 32-byte output of the BLAKE3 hash function.
- This specification defines **version 1** (“V1”) of the ZAINO UTXO snapshot.
- consensus network:
    a set of validators cooperating to consense on a blockchain


## Abstract

This document specifies a deterministic, versioned procedure to compute a 32-byte digest over a consensus network’s UTXO set.
The intent is to provide indexer operators with a utiliy for:

 * fast equality checks between independently built indices
 * reproducible debugging across indexers
 * audit logs.

The hash is _not_ input to consensus validation.

## Motivation

Different nodes (e.g., `zcashd`, Zebra, indexers) may expose distinct internals or storage layouts. Operators often need a cheap way to verify “we’re looking at the same unspent set” without transporting the entire set. A canonical, versioned snapshot hash solves this.

## Domain Separation

Implementations **MUST** domain-separate the hash with the ASCII header:

```
"ZAINO-UTXOSET-V1\0"
```

Any change to the encoding rules or semantics **MUST** bump the domain string (e.g., `…-V2\0`) and is out of scope of this document.

## Inputs

To compute the snapshot hash, the implementation needs:

- `network`: ASCII string identifying the chain. Recommended values: `"mainnet"`, `"testnet"`, `"regtest"`.
- `best_height`: the best chain height at the time of the snapshot (unsigned 32-bit).
- `best_block`: the 32-byte block hash of the best chain tip, in the node’s _canonical internal byte order_.
- `UTXO set`: a finite multimap keyed by outpoints `(txid, vout)` to outputs `(value_zat, scriptPubKey)`, where:

  - `txid` is a 32-byte transaction hash (internal byte order).
  - `vout` is a 32-bit output index (0-based).
  - `value_zat` is a non-negative amount in zatoshis, range-checked to the node’s monetary bounds (e.g., `0 ≤ value_zat ≤ MAX_MONEY`).
  - `scriptPubKey` is a byte string.

Implementations **MUST** reject negative values or out-of-range amounts prior to hashing.

## Canonical Ordering

The snapshot **MUST** be ordered as follows, independent of the node’s in-memory layout:

1. Sort by `txid` ascending, comparing the raw 32-byte values as unsigned bytes.
2. For equal `txid`s, sort by `vout` ascending (unsigned 32-bit).

This ordering **MUST** be used for serialization.

## Serialization

The byte stream fed to the hash is the concatenation of a **header** and **entries**:

### Header

- ASCII bytes: `"ZAINO-UTXOSET-V1\0"`
- `network` as ASCII bytes, followed by a single NUL byte `0x00`.
- `best_height` as `u32` little-endian.
- `best_block` as 32 raw bytes.
- `count_txouts` as `u64` little-endian, where `count_txouts` is the total number of serialized entries below.

### Entries (one per outpoint in canonical order)

For each `(txid, vout, value_zat, scriptPubKey)`:

- `txid` as 32 raw bytes.
- `vout` as `u32` little-endian.
- `value_zat` as `u64` little-endian.
- `script_len` as CompactSize (Bitcoin/Zcash varint) of `scriptPubKey.len()`.
- `scriptPubKey` raw bytes.

**Note:** No per-transaction terminators or grouping markers are used. Instead, the format commits to _outputs_, not _transactions_.

### CompactSize ([reference](https://en.bitcoin.it/wiki/Protocol_documentation#Variable_length_integer))

- If `n < 0xFD`: a single byte `n`.
- Else if `n ≤ 0xFFFF`: `0xFD` followed by `n` as `u16` little-endian.
- Else if `n ≤ 0xFFFF_FFFF`: `0xFE` followed by `n` as `u32` little-endian.
- Else: `0xFF` followed by `n` as `u64` little-endian.

## Hash Function

- The implementation **MUST** stream the bytes above into a BLAKE3 hasher.
- The 32-byte output of the hasher is the **snapshot hash**.

## Pseudocode

```text
function UtxoSnapshotHashV1(network, best_height, best_block, utxos):
    H ← blake3::Hasher()

    // Header
    H.update("ZAINO-UTXOSET-V1\0")
    H.update(network)
    H.update("\0")
    H.update(le_u32(best_height))
    H.update(best_block)         // 32 raw bytes, node’s canonical order
    count ← number_of_outputs(utxos)
    H.update(le_u64(count))

    // Entries in canonical order
    for (txid, vout, value, script) in sort_by_txid_then_vout(utxos):
        assert 0 ≤ value ≤ MAX_MONEY
        H.update(txid)                      // 32 raw bytes
        H.update(le_u32(vout))
        H.update(le_u64(value))             // zatoshis
        H.update(CompactSize(script.len))
        H.update(script)

    return H.finalize() // 32-byte BLAKE3 digest
```

## Error Handling

- If any `value_zat` is negative or exceeds `MAX_MONEY`, the snapshot procedure **MUST** fail and **MUST NOT** produce a hash.
- If the UTXO set changes during iteration (non-atomic read), the implementation **SHOULD** retry using a stable view (e.g., read lock or height-pinned snapshot).

## Security and Interop Considerations

- This hash is **not a consensus commitment** and **MUST NOT** be used to validate blocks or transactions.
- The domain string prevents cross-protocol collisions.
- Including `network`, `best_height`, and `best_block` prevents accidental equality across different nodes or heights.
- Because the order is fully specified, two independent implementations reading the _same_ set will produce the _same_ hash.

## Rationale

- **BLAKE3** is chosen for speed and strong modern security. SHA-256 would also work but is slower in large sets. The domain string ensures local uniqueness regardless of the hash function family.
- Committing to _outputs_ rather than _transactions_ simplifies implementations that don’t have transaction-grouped storage.
- CompactSize matches existing Bitcoin/Zcash encoding and avoids ambiguity.

## Versioning

- Any breaking change to the byte stream, input semantics, or ordering **MUST** bump the domain tag to `ZAINO-UTXOSET-V2\0` (or higher).
- Implementations **SHOULD** publish the version alongside the hash in logs and APIs.

## Test Guidance

Implementations **SHOULD** include tests covering:

1. **Determinism:** Shuffle input, and the hash remains constant.
2. **Sensitivity:** Flip one bit in `value_zat` or `scriptPubKey`, and the hash changes.
3. **Metadata:** Change `network` or `best_block`, and the hash changes.
4. **Empty Set:** With `count_txouts = 0`, the hash is well-defined.
5. **Large Scripts:** Scripts with CompactSize boundaries (252, 253, 2^16, 2^32).
6. **Ordering:** Two entries with same `txid` different `vout` are ordered by `vout`.

## References

[^BCP14]: [Information on BCP 14 — "RFC 2119"](https://www.rfc-editor.org/info/bcp14)
