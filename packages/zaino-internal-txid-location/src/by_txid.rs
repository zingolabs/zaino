//! `by_txid` map: txid (protocol byte order) → height u32 BE | index u32 BE | block hash 32

use zaino_persistence::MapTable;
use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

const TXID: usize = 32;
pub(crate) const MINED: usize = 4 + 4 + 32;
pub(crate) const BY_TXID: MapTable = MapTable::point(0, "by_txid", TXID as u32, MINED as u32);

/// Where one transaction was mined: its block, and its position in it (coinbase = 0)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mined {
    pub block: BlockRef,
    pub index: u32,
}

pub(crate) fn encode(mined: Mined) -> [u8; MINED] {
    let mut bytes = [0; MINED];
    bytes[..4].copy_from_slice(&u32::from(mined.block.height).to_be_bytes());
    bytes[4..8].copy_from_slice(&mined.index.to_be_bytes());
    bytes[8..].copy_from_slice(&mined.block.hash.0);
    bytes
}

pub(crate) fn decode(bytes: &[u8; MINED]) -> Mined {
    let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
    let hash = BlockHash(bytes[8..].try_into().expect("32 bytes"));
    Mined { block: BlockRef { hash, height: BlockHeight::from(word(0)) }, index: word(4) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mined_location_is_its_golden_bytes() {
        let block =
            BlockRef { hash: BlockHash([0xAB; 32]), height: BlockHeight::from(0x0102_0304) };
        let mined = Mined { block, index: 0x0506_0708 };
        let golden =
            [[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08].as_slice(), &[0xAB; 32]].concat();
        assert_eq!(encode(mined).as_slice(), golden.as_slice());
        assert_eq!(decode(&encode(mined)), mined);
    }
}
