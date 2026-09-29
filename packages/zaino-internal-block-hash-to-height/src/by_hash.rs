//! One segment row (`hash ‖ height u32 BE`) and its key
//!
//! - `PROBED`: segments carry a filter, sharded on the key's first 8 bytes (uniform for a hash)

use zaino_persistence::lsm::{Key, Record};

use crate::HASH;

/// Block hash, protocol byte order
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HashKey(pub(crate) [u8; HASH]);

impl Key for HashKey {
    const LEN: usize = HASH;
    const PROBED: bool = true;

    fn encode(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self(bytes.get(..HASH)?.try_into().ok()?))
    }
}

/// `hash ‖ height u32 BE`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HashRow {
    pub(crate) hash: HashKey,
    pub(crate) height: u32,
}

impl Record for HashRow {
    type Key = HashKey;

    const STRIDE: usize = HASH + 4;

    fn key(&self) -> HashKey {
        self.hash
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.hash.0);
        out.extend_from_slice(&self.height.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            hash: HashKey::decode(bytes)?,
            height: u32::from_be_bytes(bytes.get(HASH..HASH + 4)?.try_into().ok()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_golden_bytes_round_trip() {
        let row = HashRow { hash: HashKey([0xab; HASH]), height: 0x0102_0304 };
        let mut bytes = Vec::new();
        row.encode(&mut bytes);
        assert_eq!(bytes, [&[0xab; HASH][..], &[1, 2, 3, 4]].concat());
        assert_eq!(HashRow::decode(&bytes), Some(row));
        assert_eq!(HashRow::decode(&bytes[..HASH + 3]), None);
    }
}
