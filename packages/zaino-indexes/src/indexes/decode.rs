//! A small forward byte cursor for index `decode_value` implementations.
//!
//! The per-pool indexes persist length-prefixed, nested structures; decoding
//! them safely means bounds-checking every read. This cursor centralises that:
//! every `take` is checked, so a short or malformed buffer yields a
//! [`DecodeError`] rather than a panic.

use zaino_persistence_codec::DecodeError;

/// A forward cursor over persisted index bytes.
pub(crate) struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of `bytes`.
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
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
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self
            .take(N)?
            .try_into()
            .expect("take yields exactly N bytes"))
    }

    /// A little-endian `u32`.
    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    /// A little-endian `u64`.
    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array::<8>()?))
    }

    /// A count/length read as a `u32` and widened to `usize` for looping.
    pub(crate) fn count(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.u32()?)
            .map_err(|_| DecodeError::Invalid("count exceeds usize".to_owned()))
    }

    /// A `u32`-length-prefixed byte run.
    pub(crate) fn len_prefixed(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = self.count()?;
        Ok(self.take(len)?.to_vec())
    }

    /// Succeed only if every byte was consumed — a trailing tail is malformed.
    pub(crate) fn finish(self) -> Result<(), DecodeError> {
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
