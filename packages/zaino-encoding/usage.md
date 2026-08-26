# `zaino-encoding` — usage

Versioned encoding for records that go on disk. A leaf crate: it depends on
nothing of Zaino's, and it knows about no domain type. Implementations live with
the types they encode — for the finalised state, that is
[`zaino-chain-store-zainodb`](../zaino-chain-store-zainodb/usage.md).

```rust
use zaino_encoding::ZainoVersionedSerde;

let bytes = record.to_bytes()?;          // current version tag + body
let round_tripped = MyRecord::from_bytes(&bytes)?;
```

## Every record carries its version, and old decoders never leave

The format is one version tag byte, then a body. `Self::VERSION` is the newest
version *this build writes*; reading dispatches on the tag that is actually
there. So a database written by an older build stays readable, and that is the
entire point of the crate.

Adding a format:

1. Add `encode_vN` / `decode_vN` for the new layout.
2. Bump `const VERSION` to the new tag.
3. Point `encode_latest` / `decode_latest` at the new helpers.
4. **Leave every earlier `decode_vM` in place.** Deleting one makes every row
   written before the bump unreadable, and nothing in the type system will tell
   you.

Only implement the `encode_vN` helpers you actually need to reproduce historical
bytes; the defaults return an error, which keeps an implementation explicit
about what it can and cannot regenerate.

## Nested fields must have their version pinned

This is the rule that bites, and it is not optional.

`StoredEntry*` checksums are `blake2b256(encoded_key ‖ encoded_value)` over the
exact bytes on disk. If a record contains another `ZainoVersionedSerde` field,
producing a *historical* top-level encoding requires the inner field to encode
at its historical version too — otherwise the nested tag and body change, the
bytes differ, and the checksum fails to verify against a row that was never
corrupt.

```rust
// Right: the nested field is pinned to the version the historical writer used.
inner.serialize_with_version(&mut w, Some(version::V1))?;

// Wrong: writes whatever this build's VERSION is, silently changing the bytes.
inner.serialize(&mut w)?;
```

## Endianness is a choice each field makes

Both orders are provided because both are used. Keys that must sort correctly
under LMDB's byte comparison — block heights, above all — are big-endian;
payload fields are little-endian. Neither is a default; pick per field and match
whatever is already on disk.

## Pin new formats with golden vectors

Anything encoded here ends up in a file that outlives the build that wrote it. A
type whose encoding is not pinned by checked-in hex can be changed by a
refactor with nothing failing until a user opens an old database. Put the
vectors next to the implementation, not here.
