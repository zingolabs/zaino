//! A cursor over consensus bytes: little-endian integers, fixed-size arrays,
//! and the compact-size prefix every variable-length field carries.

use crate::error::DecodeError;

/// The largest value a compact-size prefix may carry (the protocol's
/// `MAX_SIZE`).
const MAX_COMPACT_SIZE: u64 = 0x0200_0000;

/// A position in a byte slice; every read advances it or fails without
/// moving it.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.position
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    /// The bytes read since `start`.
    pub(crate) fn since(&self, start: usize) -> &'a [u8] {
        &self.bytes[start..self.position]
    }

    /// The next `n` bytes.
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let truncated = || DecodeError::Truncated {
            needed: n,
            available: self.remaining(),
        };
        let end = self.position.checked_add(n).ok_or_else(truncated)?;
        let slice = self.bytes.get(self.position..end).ok_or_else(truncated)?;
        self.position = end;
        Ok(slice)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<(), DecodeError> {
        self.take(n).map(|_| ())
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        self.array::<1>().map(|[byte]| byte)
    }

    pub(crate) fn u32_le(&mut self) -> Result<u32, DecodeError> {
        self.array().map(u32::from_le_bytes)
    }

    pub(crate) fn i32_le(&mut self) -> Result<i32, DecodeError> {
        self.array().map(i32::from_le_bytes)
    }

    pub(crate) fn i64_le(&mut self) -> Result<i64, DecodeError> {
        self.array().map(i64::from_le_bytes)
    }

    /// A compact-size prefix, in its canonical form only: the shortest
    /// encoding that holds the value, and no value past the protocol maximum.
    pub(crate) fn compact_size(&mut self) -> Result<usize, DecodeError> {
        let value = match self.u8()? {
            flag @ 0..=252 => u64::from(flag),
            253 => {
                let value = u64::from(u16::from_le_bytes(self.array()?));
                canonical(value, 253)?
            }
            254 => {
                let value = u64::from(u32::from_le_bytes(self.array()?));
                canonical(value, 0x1_0000)?
            }
            255 => {
                let value = u64::from_le_bytes(self.array()?);
                canonical(value, 0x1_0000_0000)?
            }
        };
        if value > MAX_COMPACT_SIZE {
            return Err(DecodeError::CompactSizeTooLarge(value));
        }
        usize::try_from(value).map_err(|_| DecodeError::CompactSizeTooLarge(value))
    }

    /// A compact-size-prefixed byte string.
    pub(crate) fn var_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.compact_size()?;
        self.take(len)
    }

    /// A compact-size-prefixed vector of items, each at least `min_item` bytes
    /// long. The bound rejects a count the remaining bytes cannot possibly
    /// hold before anything is allocated for it.
    pub(crate) fn vector<T>(
        &mut self,
        min_item: usize,
        mut read: impl FnMut(&mut Reader<'a>) -> Result<T, DecodeError>,
    ) -> Result<Vec<T>, DecodeError> {
        let count = self.compact_size()?;
        let needed = count
            .checked_mul(min_item)
            .ok_or(DecodeError::CompactSizeTooLarge(u64::MAX))?;
        if needed > self.remaining() {
            return Err(DecodeError::Truncated {
                needed,
                available: self.remaining(),
            });
        }
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(read(self)?);
        }
        Ok(items)
    }
}

/// A value read through a longer compact-size form must be one the shorter
/// forms could not hold.
fn canonical(value: u64, minimum: u64) -> Result<u64, DecodeError> {
    if value < minimum {
        Err(DecodeError::NonCanonicalCompactSize)
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_size_reads_each_form_and_rejects_the_non_canonical() {
        assert_eq!(Reader::new(&[252]).compact_size().expect("one byte"), 252);
        assert_eq!(
            Reader::new(&[253, 253, 0])
                .compact_size()
                .expect("two bytes"),
            253
        );
        assert_eq!(
            Reader::new(&[254, 0, 0, 1, 0])
                .compact_size()
                .expect("four bytes"),
            0x1_0000
        );
        assert!(matches!(
            Reader::new(&[253, 252, 0]).compact_size(),
            Err(DecodeError::NonCanonicalCompactSize)
        ));
        assert!(matches!(
            Reader::new(&[254, 1, 0, 0, 2]).compact_size(),
            Err(DecodeError::CompactSizeTooLarge(0x0200_0001))
        ));
    }

    #[test]
    fn a_vector_count_the_bytes_cannot_hold_is_truncated_before_allocation() {
        // Claims 1000 items of at least 4 bytes with 2 bytes left.
        let bytes = [253, 0xe8, 0x03, 0, 0];
        let result = Reader::new(&bytes).vector(4, |r| r.u32_le());
        assert!(matches!(
            result,
            Err(DecodeError::Truncated {
                needed: 4000,
                available: 2
            })
        ));
    }

    #[test]
    fn take_past_the_end_leaves_the_position_alone() {
        let mut reader = Reader::new(&[1, 2, 3]);
        assert!(reader.take(4).is_err());
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.take(3).expect("exactly the rest"), &[1, 2, 3]);
        assert_eq!(reader.remaining(), 0);
    }
}
