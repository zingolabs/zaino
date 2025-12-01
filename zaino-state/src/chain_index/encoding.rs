//! Holds traits and primitive functions for Zaino's Serialisation schema.
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

/* ────────────────────────── Zaino Serialiser Traits ─────────────────────────── */
/// # Zaino wire-format: one-byte version tag
///
/// ## Quick summary
///
/// ┌─ byte 0 ─┬──────────── body depends on that tag ────────────┐
/// │ version  │              (little-endian by default)          │
/// └──────────┴──────────────────────────────────────────────────┘
///
/// * `Self::VERSION` = the tag **this build *writes***.
/// * On **read**, we peek at the tag:
///   * if it equals `Self::VERSION` call `decode_latest`;
///   * otherwise fall back to the relevant `decode_vN` helper
///     (defaults to “unsupported” unless overwritten).
///
/// ## Update guide.
///
/// ### Initial release (`VERSION = 1`)
/// 1. `pub struct TxV1 { … }`   // layout frozen forever
/// 2. `impl ZainoVersionedSerde for TxV1`
///    * `const VERSION = 1`
///    * `encode_body` – **v1** layout
///    * `decode_v1` – parses **v1** bytes
///    * `decode_latest` - wrapper for `Self::decode_v1`
///
/// ### Bump to v2
/// 1. `pub struct TxV2 { … }`   // new “current” layout
/// 2. `impl From<TxV1> for TxV2` // loss-less upgrade path
/// 3. `impl ZainoVersionedSerde for TxV2`
///    * `const VERSION = 2`
///    * `encode_body` – **v2** layout
///    * `decode_v1` – `TxV1::decode_latest(r).map(Self::from)`
///    * `decode_v2` – parses **v2** bytes
///    * `decode_latest` - wrapper for `Self::decode_v2`
///
/// ### Next bumps (v3, v4, …, vN)
/// * Create struct for new version.
/// * Set `const VERSION = N`.
/// * Add the `decode_vN` trait method and extend the `match` table inside **this trait** when a brand-new tag first appears.
/// * Implement `decode_vN` for N’s layout.
/// * Update `decode_latest` to wrap `decode_vN`.
/// * Implement `decode_v(N-1)`, `decode_v(N-2)`, ..., `decode_v(N-K)` for all previous versions.
///
/// ## Mandatory items per implementation
/// * `const VERSION`
/// * `encode_body`
/// * `decode_vN` — **must** parse bytes for version N, where N = [`Self::VERSION`].
/// * `decode_latest` — **must** parse `Self::VERSION` bytes.
///
/// Historical helpers (`decode_v1`, `decode_v2`, …) must be implemented
/// for compatibility with historical versions
pub trait ZainoVersionedSerde: Sized {
    /// Tag this build writes.
    const VERSION: u8;

    /*──────────── encoding ────────────*/

    /// Encode **only** the body (no tag).
    fn encode_body<W: Write>(&self, w: &mut W) -> io::Result<()>;

    /*──────────── mandatory decoder for *this* version ────────────*/

    /// Parses a body whose tag equals `Self::VERSION`.
    ///
    /// The trait implementation must wrap `decode_vN` where N = [`Self::VERSION`]
    fn decode_latest<R: Read>(r: &mut R) -> io::Result<Self>;

    /*──────────── version decoders ────────────*/
    // Add more versions here when required.

    #[inline(always)]
    #[allow(unused)]
    /// Decode an older v1 version
    fn decode_v1<R: Read>(r: &mut R) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::InvalidData, "v1 unsupported"))
    }
    #[inline(always)]
    #[allow(unused)]
    /// Decode an older v2 version
    fn decode_v2<R: Read>(r: &mut R) -> io::Result<Self> {
        Err(io::Error::new(io::ErrorKind::InvalidData, "v2 unsupported"))
    }

    /*──────────── router ────────────*/

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
        w.write_all(&[Self::VERSION])?;
        self.encode_body(&mut w)
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
        let mut buf = Vec::new();
        self.serialize(&mut buf)?;
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
