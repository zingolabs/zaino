//! Fixed-width serde primitives shared by [`PersistentRecord`](crate::PersistentRecord)
//! implementations.
//!
//! On-disk records are built from a small set of layout atoms — little-endian
//! `u32`/`u64`, fixed 32-byte arrays, and length-framed byte runs. A DTO's
//! `encode`/`decode` composes these rather than hand-rolling offset arithmetic,
//! so every read is bounds-checked and a short or malformed buffer yields a
//! [`DecodeError`] instead of a panic.
//!
//! [`Writer`] appends atoms to a growing buffer; [`Cursor`] reads them back in
//! order and [`Cursor::finish`] rejects a trailing tail. A length count is a
//! little-endian `u32`, and a length-framed run is that count followed by its
//! bytes — the two sides ([`Writer::len_prefixed`] / [`Cursor::len_prefixed`])
//! agree by construction.

use crate::DecodeError;

/// A forward append buffer for the layout atoms of a record's on-disk bytes.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty writer pre-sized for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Append a little-endian `u32`.
    pub fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a little-endian `u64`.
    pub fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a collection length/count as a little-endian `u32`. Reads back
    /// with [`Cursor::count`].
    ///
    /// The count is a `u32`, the width every on-disk format in this workspace
    /// assumes for block-bounded collections. A count that overflows it cannot
    /// be encoded faithfully and
    /// [`PersistentRecord::encode`](crate::PersistentRecord::encode) is
    /// infallible by contract — so this asserts the invariant loudly rather than
    /// silently truncating.
    pub fn count(&mut self, count: usize) {
        self.u32(u32::try_from(count).expect("collection count fits u32"));
    }

    /// Append a fixed 32-byte array.
    pub fn bytes32(&mut self, value: &[u8; 32]) {
        self.buf.extend_from_slice(value);
    }

    /// Append raw bytes with no framing — the caller owns the width.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Append a length-framed byte run: a little-endian `u32` count, then the
    /// bytes. Reads back with [`Cursor::len_prefixed`].
    ///
    /// The count is a `u32`, the width every on-disk format in this workspace
    /// assumes for block-bounded runs. A run that overflows it cannot be encoded
    /// faithfully, and [`PersistentRecord::encode`](crate::PersistentRecord::encode)
    /// is infallible by contract — so this asserts the invariant loudly rather
    /// than silently truncating the length.
    pub fn len_prefixed(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).expect("byte-run length fits u32");
        self.u32(len);
        self.raw(bytes);
    }

    /// Consume the writer, yielding the accumulated bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// A bounds-checked forward reader over a record's on-disk bytes.
pub struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Advance over `n` bytes, or fail if fewer remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| DecodeError::Invalid("length overflow".to_owned()))?;
        let slice = self.bytes.get(self.pos..end).ok_or_else(|| {
            DecodeError::Invalid(format!(
                "unexpected end of input: need {n} bytes at offset {}, have {}",
                self.pos,
                self.bytes.len(),
            ))
        })?;
        self.pos = end;
        Ok(slice)
    }

    /// A fixed-`N` byte array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self
            .take(N)?
            .try_into()
            .expect("take yields exactly N bytes"))
    }

    /// A fixed 32-byte array — the common hash/commitment width.
    pub fn bytes32(&mut self) -> Result<[u8; 32], DecodeError> {
        self.array::<32>()
    }

    /// A little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    /// A little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array::<8>()?))
    }

    /// A count/length read as a `u32` and widened to `usize` for looping.
    pub fn count(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.u32()?)
            .map_err(|_| DecodeError::Invalid("count exceeds usize".to_owned()))
    }

    /// A length-framed byte run, as written by [`Writer::len_prefixed`].
    pub fn len_prefixed(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = self.count()?;
        Ok(self.take(len)?.to_vec())
    }

    /// Succeed only if every byte was consumed — a trailing tail is malformed.
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(DecodeError::Invalid(format!(
                "trailing bytes: consumed {} of {}",
                self.pos,
                self.bytes.len(),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_layout_atoms() {
        let mut w = Writer::new();
        w.u32(0x0102_0304);
        w.u64(0x0a0b_0c0d_0e0f_1011);
        w.bytes32(&[7u8; 32]);
        w.len_prefixed(&[1, 2, 3]);
        let bytes = w.into_bytes();

        let mut c = Cursor::new(&bytes);
        assert_eq!(c.u32().expect("u32"), 0x0102_0304);
        assert_eq!(c.u64().expect("u64"), 0x0a0b_0c0d_0e0f_1011);
        assert_eq!(c.bytes32().expect("bytes32"), [7u8; 32]);
        assert_eq!(c.len_prefixed().expect("run"), vec![1, 2, 3]);
        c.finish().expect("consumed all");
    }

    #[test]
    fn a_short_buffer_fails_instead_of_panicking() {
        // Three bytes cannot satisfy a u32 (4) or u64 (8) read.
        assert!(Cursor::new(&[0u8; 3]).u32().is_err());
        assert!(Cursor::new(&[0u8; 3]).u64().is_err());
    }

    #[test]
    fn a_trailing_tail_is_rejected() {
        let mut c = Cursor::new(&[0u8; 5]);
        assert_eq!(c.u32().expect("u32"), 0);
        assert!(c.finish().is_err());
    }
}
