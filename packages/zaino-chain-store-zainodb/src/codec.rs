//! The byte encoding of every record the finalised store writes to LMDB.
//!
//! Records carry no version tag: a change to any encoding changes the database schema, and a
//! database written under another schema is rebuilt rather than read.

use corez::io::{self, Read, Write};

/// A record the finalised store encodes to and decodes from LMDB value or key bytes.
pub(crate) trait DbCodec: Sized {
    /// Writes this record's bytes to `w`.
    fn encode<W: Write>(&self, w: &mut W) -> io::Result<()>;

    /// Reads one record's bytes from `r`.
    fn decode<R: Read>(r: &mut R) -> io::Result<Self>;

    /// Encodes this record into a new buffer.
    fn to_bytes(&self) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.encode(&mut buf)?;
        Ok(buf)
    }

    /// Decodes one record from the start of `data`.
    fn from_bytes(data: &[u8]) -> io::Result<Self> {
        Self::decode(&mut corez::io::Cursor::new(data))
    }
}

/// A record whose encoding always occupies exactly [`FixedEncodedLen::ENCODED_LEN`] bytes.
pub(crate) trait FixedEncodedLen {
    /// The number of bytes every encoding of this record occupies.
    const ENCODED_LEN: usize;
}

/* ──────────────────────────── CompactSize helpers ────────────────────────────── */
/// A zcash/bitcoin CompactSize, a form of variable-length integer
pub(crate) struct CompactSize;

/// The largest value representable as a CompactSize
pub(crate) const MAX_COMPACT_SIZE: u32 = 0x0200_0000;

impl CompactSize {
    /// Reads an integer encoded in compact form.
    pub(crate) fn read<R: Read>(mut reader: R) -> io::Result<u64> {
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

    /// Reads an integer encoded in compact form and converts it to `T`, failing when it does not fit.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn read_t<R: Read, T: TryFrom<u64>>(mut reader: R) -> io::Result<T> {
        let n = Self::read(&mut reader)?;
        <T>::try_from(n).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "CompactSize value exceeds range of target type.",
            )
        })
    }

    /// Writes the provided `usize` value to the provided Writer in compact form.
    pub(crate) fn write<W: Write>(mut writer: W, size: usize) -> io::Result<()> {
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
}

/* ───────────────────────────── integer helpers ───────────────────────────── */

/// Reads a u16 in BE format.
#[inline]
pub(crate) fn read_u16_be<R: Read>(mut r: R) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}

/// Writes a u16 in BE format.
#[inline]
pub(crate) fn write_u16_be<W: Write>(mut w: W, v: u16) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Reads a u32 in LE format.
#[inline]
pub(crate) fn read_u32_le<R: Read>(mut r: R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Reads a u32 in BE format.
#[inline]
pub(crate) fn read_u32_be<R: Read>(mut r: R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

/// Writes a u32 in LE format.
#[inline]
pub(crate) fn write_u32_le<W: Write>(mut w: W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Writes a u32 in BE format.
#[inline]
pub(crate) fn write_u32_be<W: Write>(mut w: W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

/// Reads a u64 in LE format.
#[inline]
pub(crate) fn read_u64_le<R: Read>(mut r: R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Writes a u64 in LE format.
#[inline]
pub(crate) fn write_u64_le<W: Write>(mut w: W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/// Reads an i64 in LE format.
#[inline]
pub(crate) fn read_i64_le<R: Read>(mut r: R) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(i64::from_le_bytes(buf))
}

/// Writes an i64 in LE format.
#[inline]
pub(crate) fn write_i64_le<W: Write>(mut w: W, v: i64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

/* ───────────────────────────── fixed-array helpers ───────────────────────── */

/// Read exactly `N` bytes **as-is** (little-endian / “native order”).
#[inline]
pub(crate) fn read_fixed_le<const N: usize, R: Read>(mut r: R) -> io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write an `[u8; N]` **as-is** (little-endian / “native order”).
#[inline]
pub(crate) fn write_fixed_le<const N: usize, W: Write>(
    mut w: W,
    bytes: &[u8; N],
) -> io::Result<()> {
    w.write_all(bytes)
}

/* ─────────────────────────── Option<T> helpers ──────────────────────────── */

/// 0 = None, 1 = Some.
pub(crate) fn write_option<W, T, F>(mut w: W, value: &Option<T>, mut f: F) -> io::Result<()>
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
pub(crate) fn read_option<R, T, F>(mut r: R, mut f: F) -> io::Result<Option<T>>
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
pub(crate) fn write_vec<W, T, F>(mut w: W, vec: &[T], mut f: F) -> io::Result<()>
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
pub(crate) fn read_vec<R, T, F>(mut r: R, mut f: F) -> io::Result<Vec<T>>
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

#[cfg(test)]
mod tests {
    use super::*;
    use corez::io::Cursor;

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
            "reading compactsize > MAX_COMPACT_SIZE should error"
        );
    }

    #[test]
    fn compactsize_read_t_roundtrip() {
        let mut buf = Vec::new();
        CompactSize::write(&mut buf, 1000).expect("write 1000");
        let v: u32 = CompactSize::read_t(Cursor::new(&buf)).expect("read_t to u32");
        assert_eq!(v, 1000u32);
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
    fn i64_le_roundtrip() {
        let mut buf = Vec::new();
        let val: i64 = -9_001_234_567_890i64;
        write_i64_le(&mut buf, val).expect("write_i64_le");
        let r = read_i64_le(Cursor::new(&buf)).expect("read_i64_le");
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
        let items = vec![1u32, 2u32, 3u32, 0xABCDu32];
        let mut buf = Vec::new();
        write_vec(&mut buf, &items, |w, v| write_u32_le(w, *v)).expect("write_vec");
        let mut cur = Cursor::new(&buf);
        let r: Vec<u32> = read_vec(&mut cur, |r| read_u32_le(r)).expect("read_vec");
        assert_eq!(r, items);
    }
}
