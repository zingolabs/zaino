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
    fn a_mined_location_is_its_golden_bytes_and_round_trips() {
        let block =
            BlockRef { hash: BlockHash([0xab; 32]), height: BlockHeight::from(0x0102_0304) };
        let mined = Mined { block, index: 0x0506_0708 };
        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();

        let encoded = encode(mined);

        let golden: [(&str, String); 3] = [
            ("height BE", "01020304".into()),
            ("index BE", "05060708".into()),
            ("block hash", "ab".repeat(32)),
        ];
        let mut at = 0;
        for (field, expected) in golden {
            let width = expected.len() / 2;
            assert_eq!(
                encoded.get(at..at + width).map(hex),
                Some(expected),
                "{field} at byte {at}"
            );
            at += width;
        }
        assert_eq!(hex(&encoded[at..]), "", "trailing bytes after byte {at}");
        assert_eq!(decode(&encoded), mined, "decode(encode(mined))");
    }
}
