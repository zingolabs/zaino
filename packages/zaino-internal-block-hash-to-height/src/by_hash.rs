//! `by_hash` map: block hash (protocol byte order) → height u32 BE

use zaino_persistence::MapId;
use zaino_primitives::types::{Height, HeightOverflow};

pub(crate) const BY_HASH: MapId = MapId(0);
pub(crate) const HEIGHT: usize = 4;

pub(crate) fn encode_height(height: Height) -> [u8; HEIGHT] {
    u32::from(height).to_be_bytes()
}

pub(crate) fn decode_height(bytes: &[u8; HEIGHT]) -> Result<Height, HeightOverflow> {
    Height::try_from(u32::from_be_bytes(*bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_height_is_its_golden_big_endian_bytes_and_one_past_the_maximum_is_refused() {
        let height = Height::try_from(0x0102_0304u32).expect("in range");
        assert_eq!(encode_height(height), [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(decode_height(&[0x01, 0x02, 0x03, 0x04]), Ok(height));
        let overflow = decode_height(&[0x80, 0, 0, 0]).map_err(|error| error.got);
        assert_eq!(overflow, Err(1 << 31), "above 2^31 - 1");
    }
}
