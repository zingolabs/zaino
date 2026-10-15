//! `by_hash` map: block hash (protocol byte order) → height u32 BE

use zaino_persistence::MapTable;
use zcash_protocol::consensus::BlockHeight;

use crate::HASH;

pub(crate) const HEIGHT: usize = 4;
pub(crate) const BY_HASH: MapTable = MapTable::point(0, "by_hash", HASH as u32, HEIGHT as u32);

pub(crate) fn encode_height(height: BlockHeight) -> [u8; HEIGHT] {
    u32::from(height).to_be_bytes()
}

pub(crate) fn decode_height(bytes: &[u8; HEIGHT]) -> BlockHeight {
    BlockHeight::from(u32::from_be_bytes(*bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_height_is_its_golden_big_endian_bytes() {
        let height = BlockHeight::from(0x0102_0304);
        assert_eq!(encode_height(height), [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(decode_height(&[0x01, 0x02, 0x03, 0x04]), height);
    }
}
