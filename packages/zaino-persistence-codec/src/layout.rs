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
    /// [`RecordLayout::encode`](crate::RecordLayout::encode) is
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
    /// faithfully, and [`RecordLayout::encode`](crate::RecordLayout::encode)
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

/// A single on-disk layout atom: how one field of a [`PersistentRecord`]
/// (crate::PersistentRecord) crosses to and from bytes.
///
/// A DTO's `encode`/`decode` is one call per field in declaration order, so the
/// per-type byte logic lives here — one small, unit-testable impl per atom —
/// rather than in the derive macro. `#[derive(PersistentRecord)]` emits nothing
/// but a call to [`encode`](LayoutAtom::encode)/[`decode`](LayoutAtom::decode)
/// for each field, so the encode and decode sides cannot drift from each other or
/// from the struct's fields.
///
/// The atoms mirror what [`Writer`]/[`Cursor`] provide: `u8`, little-endian
/// `u32`/`u64`, any `[u8; N]` (raw `N` bytes — this covers the 32-byte
/// hash/commitment width), and `Vec<u8>` (length-framed). Big-endian integers —
/// the key-ordering case — are their own atoms, [`BeU32`]/[`BeU64`], which the
/// derive selects for a `#[persistent(be)]` field.
pub trait LayoutAtom: Sized {
    /// Append this value's on-disk bytes to `writer`.
    fn encode(&self, writer: &mut Writer);

    /// Read this value back from `cursor`, or fail on a short/malformed buffer.
    fn decode(cursor: &mut Cursor) -> Result<Self, DecodeError>;
}

impl LayoutAtom for u8 {
    fn encode(&self, writer: &mut Writer) {
        writer.raw(&[*self]);
    }

    fn decode(cursor: &mut Cursor) -> Result<Self, DecodeError> {
        Ok(cursor.array::<1>()?[0])
    }
}

impl LayoutAtom for u32 {
    fn encode(&self, writer: &mut Writer) {
        writer.u32(*self);
    }

    fn decode(cursor: &mut Cursor) -> Result<Self, DecodeError> {
        cursor.u32()
    }
}

impl LayoutAtom for u64 {
    fn encode(&self, writer: &mut Writer) {
        writer.u64(*self);
    }

    fn decode(cursor: &mut Cursor) -> Result<Self, DecodeError> {
        cursor.u64()
    }
}

impl<const N: usize> LayoutAtom for [u8; N] {
    fn encode(&self, writer: &mut Writer) {
        writer.raw(self);
    }

    fn decode(cursor: &mut Cursor) -> Result<Self, DecodeError> {
        cursor.array::<N>()
    }
}

impl LayoutAtom for Vec<u8> {
    fn encode(&self, writer: &mut Writer) {
        writer.len_prefixed(self);
    }

    fn decode(cursor: &mut Cursor) -> Result<Self, DecodeError> {
        cursor.len_prefixed()
    }
}

/// A big-endian `u32` layout atom — the key-ordering case, where lexicographic
/// byte order must match numeric order. Selected by `#[persistent(be)]` on a
/// `u32` field.
pub struct BeU32(pub u32);

impl LayoutAtom for BeU32 {
    fn encode(&self, writer: &mut Writer) {
        writer.raw(&self.0.to_be_bytes());
    }

    fn decode(cursor: &mut Cursor) -> Result<Self, DecodeError> {
        Ok(Self(u32::from_be_bytes(cursor.array::<4>()?)))
    }
}

/// A big-endian `u64` layout atom — see [`BeU32`]. Selected by
/// `#[persistent(be)]` on a `u64` field.
pub struct BeU64(pub u64);

impl LayoutAtom for BeU64 {
    fn encode(&self, writer: &mut Writer) {
        writer.raw(&self.0.to_be_bytes());
    }

    fn decode(cursor: &mut Cursor) -> Result<Self, DecodeError> {
        Ok(Self(u64::from_be_bytes(cursor.array::<8>()?)))
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

    /// Encode one atom, then decode it back, asserting equality.
    fn round_trip<A: LayoutAtom + PartialEq + core::fmt::Debug>(value: A) {
        let mut w = Writer::new();
        value.encode(&mut w);
        let bytes = w.into_bytes();
        let mut c = Cursor::new(&bytes);
        let back = A::decode(&mut c).expect("decode");
        c.finish().expect("consumed all");
        assert_eq!(back, value);
    }

    #[test]
    fn layout_atoms_round_trip() {
        round_trip(0xABu8);
        round_trip(0x2233_4455u32);
        round_trip(0x0102_0304_0506_0708u64);
        round_trip([0x7u8; 32]);
        round_trip([0x9u8; 5]);
        round_trip(vec![1u8, 2, 3]);
        round_trip(Vec::<u8>::new());
    }

    #[test]
    fn be_atoms_write_big_endian_bytes_and_round_trip() {
        let mut w = Writer::new();
        BeU32(0x0102_0304).encode(&mut w);
        BeU64(0x0102_0304_0506_0708).encode(&mut w);
        let bytes = w.into_bytes();

        // Big-endian: most-significant byte first, distinct from the LE atoms.
        let mut expected = Vec::new();
        expected.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(bytes, expected);

        let mut c = Cursor::new(&bytes);
        assert_eq!(BeU32::decode(&mut c).expect("be32").0, 0x0102_0304);
        assert_eq!(
            BeU64::decode(&mut c).expect("be64").0,
            0x0102_0304_0506_0708,
        );
        c.finish().expect("consumed all");
    }

    #[test]
    fn a_short_buffer_fails_each_atom() {
        assert!(<u32 as LayoutAtom>::decode(&mut Cursor::new(&[0u8; 3])).is_err());
        assert!(<[u8; 32] as LayoutAtom>::decode(&mut Cursor::new(&[0u8; 8])).is_err());
        // A length prefix promising more bytes than remain.
        let mut framed = 9u32.to_le_bytes().to_vec();
        framed.extend_from_slice(&[0u8; 2]);
        assert!(<Vec<u8> as LayoutAtom>::decode(&mut Cursor::new(&framed)).is_err());
    }
}
