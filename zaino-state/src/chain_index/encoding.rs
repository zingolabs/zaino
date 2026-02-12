/* ────────────────────────── Zaino Serialiser Traits ─────────────────────────── */

/*!
Provides **backward-compatible** encoding and decoding for versioned structs that implement
`ZainoVersionedSerde`.

Design goals
- Allow code to **read**  and **write** older on-disk versions.
- Enable implementers to supply compact, explicit encoders/decoders for historical versions
  without duplicating decode logic in multiple places.
- Provide a small set of *version-agnostic* APIs that consumers can call without caring
  about which concrete wire-format was used on-disk.

Wire-format overview
- Every record begins with a single **version tag byte** followed by a version-specific body.
- `Self::VERSION` is the newest implemented version this build **writes**; on read we dispatch
  using the tag.
- Implementations *must* expose encoders/decoders for the versions they support so that
  callers can reliably regenerate exact historical bytes (necessary for checksums,
  backward-compatible verification and database version migration logic).

Developer guidance (how to implement a versioned type)
1. When introducing a type, select the initial wire version:
   - `const VERSION = version::V1;` for first release.
   - Implement `encode_v1` and `decode_v1` (body-only helpers), and make `encode_latest`
     / `decode_latest` wrap v1 behaviour.
2. When bumping to a new wire-format (V2, V3, …):
   - Introduce `encode_vN` and `decode_vN` for the new layout.
   - Update `const VERSION` to the new tag (this build will now write the new tag by default).
   - Make `encode_latest` / `decode_latest` delegate to the new vN helpers.
   - Preserve `decode_v(M)` helpers for earlier M so older on-disk values remain readable.
3. For types that *contain* inner fields that themselves implement `ZainoVersionedSerde`
   (for example `BlockHeaderData` contains `BlockIndex`), **explicitly** control the inner
   field’s encoded version when producing historical top-level encodings:
   - Use `serialize_with_version` (or `to_bytes_with_version`) on the inner field to request
     the exact nested version you need.  This guarantees that `to_bytes_with_version(Some(v))`
     reproduces exact bytes that historical writers produced (critical for checksum equality).
   - Do *not* rely on the inner field’s current `serialize()` when producing historical
     top-level encodings — doing so will change nested bytes and break checksum verification.
4. Keep encode helpers narrow and faithful:
   - Implement only the `encode_vN` helpers that are required to reproduce historical
     bytes; default implementations return an error. This keeps implementations explicit
     and easy to review.

Consumer (version-agnostic) APIs
- `serialize()` / `to_bytes()` — writes the current version tag + body.
- `serialize_with_version(&mut w, version)` / `to_bytes_with_version(version)` — write a
  chosen version tag and body (useful when reproducing historical bytes).
- `deserialize()` / `from_bytes()` / `decode_body()` — read and dispatch by version tag.

Safety note
- `StoredEntry*` wrappers compute and verify checksums over the exact bytes written to disk:
  `blake2b256(encoded_key || encoded_item_bytes)`. For verification to succeed against older
  on-disk rows, an implementation MUST be able to reproduce the exact historical `encoded_item_bytes`
  (including nested fields’ tags/bodies). Use `serialize_with_version` for nested fields to
  guarantee this behaviour.
*/
#![allow(dead_code)]

use core::iter::FromIterator;
use core2::io::{self, Read, Write};

/// Wire-format version tags.
pub mod version {
    /// Tag byte for data encoded with *v1* layout.
    pub const V1: u8 = 1;

    /// Tag byte for data encoded with *v2* layout.
    pub const V2: u8 = 2;

    // Add new versions as required.
    // pub const V3: u8 = 3;
}

/* ────────────────────────── ZainoVersionedSerde ─────────────────────────── */
/// # Zaino wire-format: one-byte version tag
///
/// ## Quick summary
///
/// ┌─ byte 0 ─┬──────────── body depends on that tag ────────────┐
/// │ version  │              (little-endian by default)          │
/// └──────────┴──────────────────────────────────────────────────┘
///
/// * `Self::VERSION` is the highest (newest) version implemented in this build.
/// * On **read**, we peek at the tag:
///   * if it equals `Self::VERSION` call `decode_latest`;
///   * otherwise fall back to the relevant `decode_vN` helper
///     (defaults to “unsupported” unless overwritten).
///
/// ## Developer behaviour (implementer guidance)
///
/// When you implement a new versioned type:
/// - Provide *both* encoders and decoders for concrete versions you need to support:
///   - `encode_vN` and `decode_vN` are the *body-only* helpers for version `N`.
///   - `encode_latest` should emit the body for the current `Self::VERSION`.
///   - `decode_latest` must parse the body for the current `Self::VERSION`.
/// - On a version bump (v1 → v2):
///   1. Implement `encode_v2` and `decode_v2` for the v2 layout.
///   2. Update `const VERSION = version::V2;`.
///   3. Make `encode_latest` forward to `encode_v2` and `decode_latest` forward to `decode_v2`.
///   4. Keep `decode_v1` and `encode_v1`so existing on-disk v1 values remain readable and
///      constructable.
/// - On version bumps >v2:
///   1. Create `pub const Vn: u8 = n` struct in version mod for new version.
///   2. Add optional `encode_vn` and `decode_vn` methods to `ZainoVersionedSerde`.
///   3. Implement new `encode_vn` and `decode_vn` for the new vn layout.
///   2. Update `const VERSION = version::Vn;`.
///   3. Make `encode_latest` forward to `encode_vn` and `decode_latest` forward to `decode_vn`.
///   4. Keep existing encodes / decodes so existing on-disk v1 values remain readable and
///      constructable.
///
/// Important: **nested versioned fields.**
/// - If your type contains fields that also implement `ZainoVersionedSerde` (for example
///   `BlockHeaderData` contains `BlockIndex`), you **must** control the concrete nested
///   version when producing historical top-level encodings.
/// - Use `serialize_with_version(..., version)` or `to_bytes_with_version(version)` on the
///   nested field inside `encode_vN` to force the inner field to be encoded with a specific
///   tag/body. This guarantees that `to_bytes_with_version(Some(v))` reproduces *exactly* the
///   bytes historical writers produced (critical for checksum equality and verification).
///
/// ## Version-agnostic consumer helpers
///
/// Implementers expose the low-level `encode_vN`/`decode_vN` helpers; consumers should use
/// the version-agnostic APIs below:
/// - `serialize()` / `to_bytes()` — write the current version (latest) tag + body.
/// - `serialize_with_version(&mut w, version)` / `to_bytes_with_version(version)` — write the
///   chosen version tag and body (use when reproducing historical bytes).
/// - `deserialize()` / `from_bytes()` — read version tag and dispatch to the correct decode.
///
/// ## Mandatory items per implementation
/// * `const VERSION`
/// * `encode_latest`
/// * `encode_vN`
/// * `decode_latest`
/// * `decode_vN`
pub trait ZainoVersionedSerde: Sized {
    /// Tag this build writes.
    const VERSION: u8;

    /*──────────── encoding ────────────*/

    /// Endodes a body whose tag equals `Self::VERSION`.
    ///
    /// The trait implementation must wrap `decode_vN` where N = [`Self::VERSION`]
    fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()>;

    /*──────────── mandatory decoder for *this* version ────────────*/

    /// Parses a body whose tag equals `Self::VERSION`.
    ///
    /// The trait implementation must wrap `decode_vN` where N = [`Self::VERSION`]
    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self>;

    /*──────────── version encoders / decoders ────────────*/
    // Add more versions here when required.

    /// Encode the body in the *v1* layout (tag-less body only).
    #[inline(always)]
    #[allow(unused)]
    fn encode_v1<W: Write>(&self, _w: &mut W) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "v1 encode unsupported",
        ))
    }

    /// Encode the body in the *v2* layout (tag-less body only).
    #[inline(always)]
    #[allow(unused)]
    fn encode_v2<W: Write>(&self, _w: &mut W) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "v2 encode unsupported",
        ))
    }

    #[inline(always)]
    #[allow(unused)]
    /// Decode the body in the *v1* layout (tag-less body only).
    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::InvalidData, "v1 unsupported"))
    }
    #[inline(always)]
    #[allow(unused)]
    /// Decode the body in the *v2* layout (tag-less body only).
    fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::InvalidData, "v2 unsupported"))
    }

    /*──────────── router ────────────*/

    /// Encode a body for the requested `version_tag`.
    ///
    /// This mirrors `decode_body` but for encoding. Types that only support latest
    /// may rely on the default behaviour where attempts to encode an older version
    /// return an error.
    #[inline]
    fn encode_body<W: Write>(&self, w: &mut W, version_tag: u8) -> io::Result<()> {
        if version_tag == Self::VERSION {
            self.encode_latest(w)
        } else {
            match version_tag {
                version::V1 => self.encode_v1(w),
                version::V2 => self.encode_v2(w),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported encode version {version_tag}"),
                )),
            }
        }
    }

    #[inline]
    /// Decode the body, dispatcing to the appropriate decode_vx function
    fn decode_body<R: Read>(r: &mut R, version_tag: u8) -> io::Result<Self> {
        if version_tag == Self::VERSION {
            Self::decode_latest(r)
        } else {
            match version_tag {
                version::V1 => Self::decode_v1(r),
                version::V2 => Self::decode_v2(r),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported Zaino version tag {version_tag}"),
                )),
            }
        }
    }

    /*──────────── User entry points ────────────*/

    #[inline]
    /// The expected start point. Read the version tag, then decode the rest
    fn serialize<W: Write>(&self, mut w: W) -> io::Result<()> {
        self.serialize_with_version(&mut w, Self::VERSION)
    }

    /// Serialise specifying a `version` (None -> latest). This writes the
    /// version tag byte and then the body encoded *for that version*.
    #[inline]
    fn serialize_with_version<W: Write>(&self, mut w: W, version: u8) -> io::Result<()> {
        w.write_all(&[version])?;
        self.encode_body(&mut w, version)
    }

    #[inline]
    /// Deserialises struct.
    fn deserialize<R: Read>(mut r: R) -> io::Result<Self> {
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag)?;
        Self::decode_body(&mut r, tag[0])
    }

    /// Serialize into a `Vec<u8>` (tag + body).
    #[inline]
    fn to_bytes(&self) -> io::Result<Vec<u8>> {
        self.to_bytes_with_version(Self::VERSION)
    }

    /// to_bytes with explicit version selection (None -> latest).
    #[inline]
    fn to_bytes_with_version(&self, version: u8) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.serialize_with_version(&mut buf, version)?;
        Ok(buf)
    }

    /// Reconstruct from a `&[u8]` (expects tag + body).
    #[inline]
    fn from_bytes(data: &[u8]) -> io::Result<Self> {
        let mut cursor = core2::io::Cursor::new(data);
        Self::deserialize(&mut cursor)
    }
}

/// Defines the fixed encoded length of a database record.
pub trait FixedEncodedLen {
    /// the fixed encoded length of a database record *not* incuding the version byte.
    const ENCODED_LEN: usize;

    /// Length of version tag in bytes.
    const VERSION_TAG_LEN: usize = 1;

    /// the fixed encoded length of a database record *incuding* the version byte.
    const VERSIONED_LEN: usize = Self::ENCODED_LEN + Self::VERSION_TAG_LEN;
}

/* ──────────────────────────── CompactSize helpers ────────────────────────────── */
/// A zcash/bitcoin CompactSize, a form of variable-length integer
pub struct CompactSize;

/// The largest value representable as a CompactSize
pub const MAX_COMPACT_SIZE: u32 = 0x0200_0000;

impl CompactSize {
    /// Reads an integer encoded in compact form.
    pub fn read<R: Read>(mut reader: R) -> io::Result<u64> {
        let mut flag_bytes = [0; 1];
        reader.read_exact(&mut flag_bytes)?;
        let flag = flag_bytes[0];

        let result = if flag < 253 {
            Ok(flag as u64)
        } else if flag == 253 {
            let mut bytes = [0; 2];
            reader.read_exact(&mut bytes)?;
            match u16::from_le_bytes(bytes) {
                n if n < 253 => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "non-canonical CompactSize",
                )),
                n => Ok(n as u64),
            }
        } else if flag == 254 {
            let mut bytes = [0; 4];
            reader.read_exact(&mut bytes)?;
            match u32::from_le_bytes(bytes) {
                n if n < 0x10000 => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "non-canonical CompactSize",
                )),
                n => Ok(n as u64),
            }
        } else {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes)?;
            match u64::from_le_bytes(bytes) {
                n if n < 0x100000000 => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "non-canonical CompactSize",
                )),
                n => Ok(n),
            }
        }?;

        match result {
            s if s > <u64>::from(MAX_COMPACT_SIZE) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CompactSize too large",
            )),
            s => Ok(s),
        }
    }

    /// Reads an integer encoded in compact form and performs checked conversion
    /// to the target type.
    pub fn read_t<R: Read, T: TryFrom<u64>>(mut reader: R) -> io::Result<T> {
        let n = Self::read(&mut reader)?;
        <T>::try_from(n).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "CompactSize value exceeds range of target type.",
            )
        })
    }

    /// Writes the provided `usize` value to the provided Writer in compact form.
    pub fn write<W: Write>(mut writer: W, size: usize) -> io::Result<()> {
        match size {
            s if s < 253 => writer.write_all(&[s as u8]),
            s if s <= 0xFFFF => {
                writer.write_all(&[253])?;
                writer.write_all(&(s as u16).to_le_bytes())
            }
            s if s <= 0xFFFFFFFF => {
                writer.write_all(&[254])?;
                writer.write_all(&(s as u32).to_le_bytes())
            }
            s => {
                writer.write_all(&[255])?;
                writer.write_all(&(s as u64).to_le_bytes())
            }
        }
    }

    /// Returns the number of bytes needed to encode the given size in compact form.
    pub fn serialized_size(size: usize) -> usize {
        match size {
            s if s < 253 => 1,
            s if s <= 0xFFFF => 3,
            s if s <= 0xFFFFFFFF => 5,
            _ => 9,
        }
    }
}

/* ───────────────────────────── integer helpers ───────────────────────────── */

/// Reads a u8.
#[inline]
pub fn read_u8<R: Read>(mut r: R) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

/// Writes a u8.
#[inline]
pub fn write_u8<W: Write>(mut w: W, v: u8) -> io::Result<()> {
    w.write_all(&[v])
}

/// Reads a u16 in LE format.
#[inline]
pub fn read_u16_le<R: Read>(mut r: R) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

/// Reads a u16 in BE format.
#[inline]
pub fn read_u16_be<R: Read>(mut r: R) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}

/// Writes a u16 in LE format.
#[inline]
pub fn write_u16_le<W: Write>(mut w: W, v: u16) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Writes a u16 in BE format.
#[inline]
pub fn write_u16_be<W: Write>(mut w: W, v: u16) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Reads a u32 in LE format.
#[inline]
pub fn read_u32_le<R: Read>(mut r: R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Reads a u32 in BE format.
#[inline]
pub fn read_u32_be<R: Read>(mut r: R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

/// Writes a u32 in LE format.
#[inline]
pub fn write_u32_le<W: Write>(mut w: W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Writes a u32 in BE format.
#[inline]
pub fn write_u32_be<W: Write>(mut w: W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Reads a u64 in LE format.
#[inline]
pub fn read_u64_le<R: Read>(mut r: R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Reads a u64 in BE format.
#[inline]
pub fn read_u64_be<R: Read>(mut r: R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

/// Writes a u64 in LE format.
#[inline]
pub fn write_u64_le<W: Write>(mut w: W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Writes a u64 in BE format.
#[inline]
pub fn write_u64_be<W: Write>(mut w: W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Reads an i64 in LE format.
#[inline]
pub fn read_i64_le<R: Read>(mut r: R) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(i64::from_le_bytes(buf))
}

/// Reads an i64 in BE format.
#[inline]
pub fn read_i64_be<R: Read>(mut r: R) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(i64::from_be_bytes(buf))
}

/// Writes an i64 in LE format.
#[inline]
pub fn write_i64_le<W: Write>(mut w: W, v: i64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Writes an i64 in BE format.
#[inline]
pub fn write_i64_be<W: Write>(mut w: W, v: i64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/* ───────────────────────────── fixed-array helpers ───────────────────────── */

/// Read exactly `N` bytes **as-is** (little-endian / “native order”).
#[inline]
pub fn read_fixed_le<const N: usize, R: Read>(mut r: R) -> io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write an `[u8; N]` **as-is** (little-endian / “native order”).
#[inline]
pub fn write_fixed_le<const N: usize, W: Write>(mut w: W, bytes: &[u8; N]) -> io::Result<()> {
    w.write_all(bytes)
}

/// Read exactly `N` bytes from the stream and **reverse** them so the caller
/// receives little-endian/internal order while the wire sees big-endian.
#[inline]
pub fn read_fixed_be<const N: usize, R: Read>(mut r: R) -> io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf)?;
    buf.reverse();
    Ok(buf)
}

/// Take an internal little-endian `[u8; N]`, reverse it, and write big-endian
/// order to the stream.
#[inline]
pub fn write_fixed_be<const N: usize, W: Write>(mut w: W, bytes: &[u8; N]) -> io::Result<()> {
    let mut tmp = *bytes;
    tmp.reverse();
    w.write_all(&tmp)
}

/* ─────────────────────────── Option<T> helpers ──────────────────────────── */

/// 0 = None, 1 = Some.
pub fn write_option<W, T, F>(mut w: W, value: &Option<T>, mut f: F) -> io::Result<()>
where
    W: Write,
    F: FnMut(&mut W, &T) -> io::Result<()>,
{
    match value {
        None => w.write_all(&[0]),
        Some(val) => {
            w.write_all(&[1])?;
            f(&mut w, val)
        }
    }
}

/// Reads an option based on option tag byte.
pub fn read_option<R, T, F>(mut r: R, mut f: F) -> io::Result<Option<T>>
where
    R: Read,
    F: FnMut(&mut R) -> io::Result<T>,
{
    let mut flag = [0u8; 1];
    r.read_exact(&mut flag)?;
    match flag[0] {
        0 => Ok(None),
        1 => f(&mut r).map(Some),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "non-canonical Option tag",
        )),
    }
}

/* ──────────────────────────── Vec<T> helpers ────────────────────────────── */
/// Writes a vec of structs, preceded by number of items (compactsize).
pub fn write_vec<W, T, F>(mut w: W, vec: &[T], mut f: F) -> io::Result<()>
where
    W: Write,
    F: FnMut(&mut W, &T) -> io::Result<()>,
{
    CompactSize::write(&mut w, vec.len())?;
    for item in vec {
        f(&mut w, item)?
    }
    Ok(())
}

/// Reads a vec of structs, preceded by number of items (compactsize).
pub fn read_vec<R, T, F>(mut r: R, mut f: F) -> io::Result<Vec<T>>
where
    R: Read,
    F: FnMut(&mut R) -> io::Result<T>,
{
    let len = CompactSize::read(&mut r)? as usize;
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        v.push(f(&mut r)?);
    }
    Ok(v)
}

/// Same as `read_vec` but collects straight into any container that
/// implements `FromIterator`.
pub fn read_vec_into<R, T, C, F>(mut r: R, mut f: F) -> io::Result<C>
where
    R: Read,
    F: FnMut(&mut R) -> io::Result<T>,
    C: FromIterator<T>,
{
    let len = CompactSize::read(&mut r)? as usize;
    (0..len).map(|_| f(&mut r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core2::io::Cursor;

    #[test]
    fn compactsize_roundtrip_various() {
        let values: &[usize] = &[
            0usize,
            1,
            10,
            252,
            253,
            254,
            1024,
            0xFFFFusize,
            0x1_0000usize,
            MAX_COMPACT_SIZE as usize,
        ];

        for &v in values {
            let mut buf = Vec::new();
            CompactSize::write(&mut buf, v).expect("write compactsize");
            let mut cur = Cursor::new(&buf);
            let r = CompactSize::read(&mut cur).expect("read compactsize");
            assert_eq!(r as usize, v, "compactsize roundtrip mismatch for {}", v);

            // serialized_size should match produced length
            assert_eq!(CompactSize::serialized_size(v), buf.len());
        }
    }

    #[test]
    fn compactsize_too_large_errors() {
        let too_big = (MAX_COMPACT_SIZE as usize) + 1;
        let mut buf = Vec::new();
        CompactSize::write(&mut buf, too_big).expect("write oversized");
        // Reading should return an error because the value exceeds MAX_COMPACT_SIZE.
        assert!(
            CompactSize::read(Cursor::new(&buf)).is_err(),
            "reading compactsize > MAX_COMPAC T_SIZE should error"
        );
    }

    #[test]
    fn compactsize_read_t_roundtrip() {
        let mut buf = Vec::new();
        CompactSize::write(&mut buf, 1000).expect("write 1000");
        let mut cur = Cursor::new(&buf);
        let v: u32 = CompactSize::read_t(&mut cur).expect("read_t to u32");
        assert_eq!(v, 1000u32);
    }

    #[test]
    fn u8_roundtrip() {
        let mut buf = Vec::new();
        write_u8(&mut buf, 0xAB).expect("write_u8");
        let v = read_u8(Cursor::new(&buf)).expect("read_u8");
        assert_eq!(v, 0xAB);
    }

    #[test]
    fn u16_le_roundtrip() {
        let mut buf = Vec::new();
        write_u16_le(&mut buf, 0x1234).expect("write_u16_le");
        let v = read_u16_le(Cursor::new(&buf)).expect("read_u16_le");
        assert_eq!(v, 0x1234);
    }

    #[test]
    fn u16_be_roundtrip() {
        let mut buf = Vec::new();
        write_u16_be(&mut buf, 0x1234).expect("write_u16_be");
        let v = read_u16_be(Cursor::new(&buf)).expect("read_u16_be");
        assert_eq!(v, 0x1234);
    }

    #[test]
    fn u32_le_roundtrip() {
        let mut buf = Vec::new();
        write_u32_le(&mut buf, 0x1122_3344).expect("write_u32_le");
        let v = read_u32_le(Cursor::new(&buf)).expect("read_u32_le");
        assert_eq!(v, 0x1122_3344);
    }

    #[test]
    fn u32_be_roundtrip() {
        let mut buf = Vec::new();
        write_u32_be(&mut buf, 0x1122_3344).expect("write_u32_be");
        let v = read_u32_be(Cursor::new(&buf)).expect("read_u32_be");
        assert_eq!(v, 0x1122_3344);
    }

    #[test]
    fn u64_le_roundtrip() {
        let mut buf = Vec::new();
        write_u64_le(&mut buf, 0x0102_0304_0506_0708).expect("write_u64_le");
        let v = read_u64_le(Cursor::new(&buf)).expect("read_u64_le");
        assert_eq!(v, 0x0102_0304_0506_0708u64);
    }

    #[test]
    fn u64_be_roundtrip() {
        let mut buf = Vec::new();
        write_u64_be(&mut buf, 0x0102_0304_0506_0708).expect("write_u64_be");
        let v = read_u64_be(Cursor::new(&buf)).expect("read_u64_be");
        assert_eq!(v, 0x0102_0304_0506_0708u64);
    }

    #[test]
    fn i64_le_roundtrip() {
        let mut buf = Vec::new();
        let val: i64 = -9_001_234_567_890i64;
        write_i64_le(&mut buf, val).expect("write_i64_le");
        let r = read_i64_le(Cursor::new(&buf)).expect("read_i64_le");
        assert_eq!(r, val);
    }

    #[test]
    fn i64_be_roundtrip() {
        let mut buf = Vec::new();
        let val: i64 = -9_001_234_567_890i64;
        write_i64_be(&mut buf, val).expect("write_i64_be");
        let r = read_i64_be(Cursor::new(&buf)).expect("read_i64_be");
        assert_eq!(r, val);
    }

    #[test]
    fn fixed_le_roundtrip() {
        let arr: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut buf = Vec::new();
        write_fixed_le::<8, _>(&mut buf, &arr).expect("write_fixed_le");
        let got: [u8; 8] = read_fixed_le::<8, _>(Cursor::new(&buf)).expect("read_fixed_le");
        assert_eq!(got, arr);
    }

    #[test]
    fn fixed_be_roundtrip() {
        let arr: [u8; 8] = [10, 11, 12, 13, 14, 15, 16, 17];
        let mut buf = Vec::new();
        write_fixed_be::<8, _>(&mut buf, &arr).expect("write_fixed_be");
        let got: [u8; 8] = read_fixed_be::<8, _>(Cursor::new(&buf)).expect("read_fixed_be");
        // read_fixed_be reverses the wire bytes back into internal order, so we expect equality
        assert_eq!(got, arr);
    }

    #[test]
    fn option_none_roundtrip() {
        let mut buf = Vec::new();
        write_option(&mut buf, &None::<u32>, |_w, _v| Ok(())).expect("write_option none");
        let mut cur = Cursor::new(&buf);
        let r: Option<u32> = read_option(&mut cur, |_r| unreachable!()).expect("read_option none");
        assert!(r.is_none());
    }

    #[test]
    fn option_some_roundtrip() {
        let mut buf = Vec::new();
        write_option(&mut buf, &Some(0xDEADBEEFu32), |w, v| write_u32_le(w, *v))
            .expect("write_option some");
        let mut cur = Cursor::new(&buf);
        let r: Option<u32> = read_option(&mut cur, |r| read_u32_le(r)).expect("read_option some");
        assert_eq!(r, Some(0xDEADBEEF));
    }

    #[test]
    fn write_vec_read_vec_roundtrip() {
        let items = vec![1u16, 2u16, 3u16, 0xABCDu16];
        let mut buf = Vec::new();
        write_vec(&mut buf, &items, |w, v| write_u16_le(w, *v)).expect("write_vec");
        let mut cur = Cursor::new(&buf);
        let r: Vec<u16> = read_vec(&mut cur, |r| read_u16_le(r)).expect("read_vec");
        assert_eq!(r, items);
    }

    #[test]
    fn read_vec_into_roundtrip() {
        let items = vec![10u32, 11u32, 12u32];
        let mut buf = Vec::new();
        write_vec(&mut buf, &items, |w, v| write_u32_le(w, *v)).expect("write_vec u32");
        let mut cur = Cursor::new(&buf);
        let out: Vec<u32> = read_vec_into(&mut cur, |r| read_u32_le(r)).expect("read_vec_into");
        assert_eq!(out, items);
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Inner {
        pub x: u32,
    }

    // Inner: versioned: v1 writes little-endian, v2 writes big-endian for demonstration.
    impl ZainoVersionedSerde for Inner {
        // current build writes v2
        const VERSION: u8 = version::V2;

        fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
            // latest = v2
            self.encode_v2(w)
        }

        fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
            Self::decode_v2(r)
        }

        fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
            // v1 body: x as little-endian
            write_u32_le(w, self.x)
        }

        fn encode_v2<W: Write>(&self, w: &mut W) -> io::Result<()> {
            // v2 body: x as big-endian
            write_u32_be(w, self.x)
        }

        fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
            let x = read_u32_le(r)?;
            Ok(Inner { x })
        }

        fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
            let x = read_u32_be(r)?;
            Ok(Inner { x })
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Outer {
        pub inner: Inner,
        pub y: u8,
    }

    // Outer: top-level is versioned. Top-level v1 must include nested Inner encoded as v1.
    impl ZainoVersionedSerde for Outer {
        // this build writes v2 by default
        const VERSION: u8 = version::V2;

        fn encode_latest<W: Write>(&self, w: &mut W) -> io::Result<()> {
            self.encode_v2(w)
        }

        fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self> {
            Self::decode_v2(r)
        }

        // Top-level v1: explicitly request inner as v1 (writes inner tag+body for v1)
        fn encode_v1<W: Write>(&self, w: &mut W) -> io::Result<()> {
            // write nested Inner as a versioned record with tag+body
            self.inner.serialize_with_version(&mut *w, version::V1)?;
            // then write y
            w.write_all(&[self.y])?;
            Ok(())
        }

        // Top-level v2: explicitly request inner as v2
        fn encode_v2<W: Write>(&self, w: &mut W) -> io::Result<()> {
            self.inner.serialize_with_version(&mut *w, version::V2)?;
            w.write_all(&[self.y])?;
            Ok(())
        }

        fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
            // inner is stored with its own tag, so use Inner::deserialize
            let inner = Inner::deserialize(&mut *r)?;
            let mut buf = [0u8; 1];
            r.read_exact(&mut buf)?;
            Ok(Outer { inner, y: buf[0] })
        }

        fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
            // same decoding behavior: nested inner may include its tag
            let inner = Inner::deserialize(&mut *r)?;
            let mut buf = [0u8; 1];
            r.read_exact(&mut buf)?;
            Ok(Outer { inner, y: buf[0] })
        }
    }

    #[test]
    fn inner_v1_v2_roundtrip_and_difference() {
        let i = Inner { x: 0x1122_3344 };

        // v1 and v2 bytes should differ
        let b_v1 = i.to_bytes_with_version(version::V1).expect("v1 bytes");
        let b_v2 = i.to_bytes_with_version(version::V2).expect("v2 bytes");
        assert_ne!(b_v1, b_v2, "v1 and v2 encodings must differ for the test");

        // decoding roundtrip for both
        let decoded_v1 = Inner::from_bytes(&b_v1).expect("decode v1");
        let decoded_v2 = Inner::from_bytes(&b_v2).expect("decode v2");
        assert_eq!(decoded_v1, i);
        assert_eq!(decoded_v2, i);

        // check tags: first byte is version tag
        assert_eq!(b_v1[0], version::V1);
        assert_eq!(b_v2[0], version::V2);

        // Inspect body bytes to ensure endianness was used differently
        // body starts at index 1 (after tag)
        let body_v1 = &b_v1[1..];
        let body_v2 = &b_v2[1..];
        // v1 is little-endian -> first body byte should be low-order byte 0x44
        assert_eq!(body_v1[0], 0x44);
        // v2 is big-endian -> first body byte should be high-order byte 0x11
        assert_eq!(body_v2[0], 0x11);
    }

    #[test]
    fn outer_nested_v1_v2_roundtrip_and_nested_tag_behavior() {
        let o = Outer {
            inner: Inner { x: 0xAABBCCDD },
            y: 0x7f,
        };

        // produce top-level v1 and v2 bytes
        let top_v1 = o.to_bytes_with_version(version::V1).expect("outer v1");
        let top_v2 = o.to_bytes_with_version(version::V2).expect("outer v2");

        // top-level tags:
        assert_eq!(top_v1[0], version::V1);
        assert_eq!(top_v2[0], version::V2);

        // nested inner tag should appear immediately after top-level tag
        // (we wrote inner with serialize_with_version in encode_vN)
        assert!(top_v1.len() >= 2 && top_v2.len() >= 2);
        let nested_tag_v1 = top_v1[1];
        let nested_tag_v2 = top_v2[1];

        // For top-level v1 we explicitly asked inner to be v1
        assert_eq!(nested_tag_v1, version::V1);
        // For top-level v2 we explicitly asked inner to be v2
        assert_eq!(nested_tag_v2, version::V2);

        // decode both and ensure roundtrip equality
        let decoded_v1 = Outer::from_bytes(&top_v1).expect("decode outer v1");
        let decoded_v2 = Outer::from_bytes(&top_v2).expect("decode outer v2");
        assert_eq!(decoded_v1, o);
        assert_eq!(decoded_v2, o);
    }

    #[test]
    fn serialize_and_deserialize_helpers_consistency() {
        let i = Inner { x: 0x0102_0304 };

        // serialize() should use latest = v2
        let latest_bytes = i.to_bytes().expect("latest bytes");
        assert_eq!(latest_bytes[0], version::V2);

        // serialize_with_version should produce requested tag
        let v1_bytes = i.to_bytes_with_version(version::V1).expect("v1 bytes");
        let v2_bytes = i.to_bytes_with_version(version::V2).expect("v2 bytes");
        assert_eq!(v1_bytes[0], version::V1);
        assert_eq!(v2_bytes[0], version::V2);

        // to_bytes/from_bytes roundtrip across versions
        assert_eq!(Inner::from_bytes(&v1_bytes).expect("from v1"), i);
        assert_eq!(Inner::from_bytes(&v2_bytes).expect("from v2"), i);
        // latest roundtrip
        assert_eq!(Inner::from_bytes(&latest_bytes).expect("from latest"), i);
    }

    // Additional test: ensure nested explicit encoding is necessary
    #[test]
    fn nested_encoding_must_use_serialize_with_version() {
        let o = Outer {
            inner: Inner { x: 0xDEAD_BEEF },
            y: 0x42,
        };

        // If we had implemented encode_v1 for Outer *without* calling inner.serialize_with_version(..., V1)
        // the nested tag would be the inner's current tag (V2), and roundtrip of top-level v1
        // produced by such a broken implementation would not match historical bytes.
        //
        // The test below asserts that our encode_v1 produces nested tag == V1.
        let top_v1 = o.to_bytes_with_version(version::V1).expect("outer v1");
        assert_eq!(
            top_v1[1],
            version::V1,
            "nested inner tag must be v1 for top-level v1"
        );

        // Confirm the outer decoding still works
        let decoded = Outer::from_bytes(&top_v1).expect("decode outer v1");
        assert_eq!(decoded, o);
    }
}
