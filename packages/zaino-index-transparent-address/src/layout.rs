//! Disk layouts of the two maps (`receives`, `spent`) + the records they hold
//!
//! - locators (height, vout) big-endian (keys compare as bytes: byte order = key order)
//! - receive value = little-endian quantity
//! - derived `Ord` = key byte order (pinned by a test below)

use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::Zatoshis;
use zcash_protocol::TxId;

use crate::address::{AddressKey, AddressKind, HASH160};

const TXID: usize = 32;

pub(crate) const RECEIVE_KEY: usize = AddressKey::LEN + 4 + TXID + 4;
pub(crate) const RECEIVE_VALUE: usize = 8;
pub(crate) const SPENT_KEY: usize = TXID + 4;
pub(crate) const SPEND: usize = 4 + TXID;

/// `addr ‖ height ‖ txid ‖ vout` (range-scanned per address: height directly after it)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReceiveKey {
    pub(crate) address: AddressKey,
    pub(crate) height: BlockHeight,
    pub(crate) txid: TxId,
    pub(crate) vout: u32,
}

impl ReceiveKey {
    /// Lowest key `address` can hold at `height` (a scan bound, never a stored row)
    pub(crate) fn first(address: AddressKey, height: BlockHeight) -> Self {
        Self { address, height, txid: TxId::from_bytes([0u8; TXID]), vout: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReceiveRow {
    pub(crate) key: ReceiveKey,
    pub(crate) value: Zatoshis,
}

/// What spent an outpoint (a `spent` row's value)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spend {
    pub(crate) height: BlockHeight,
    pub(crate) spender: TxId,
}

pub(crate) fn encode_receive_key(key: &ReceiveKey) -> [u8; RECEIVE_KEY] {
    let mut out = [0u8; RECEIVE_KEY];
    out[..HASH160].copy_from_slice(&key.address.hash);
    out[HASH160] = key.address.kind.encode();
    let at = AddressKey::LEN;
    out[at..at + 4].copy_from_slice(&u32::from(key.height).to_be_bytes());
    out[at + 4..at + 4 + TXID].copy_from_slice(key.txid.as_ref());
    out[at + 4 + TXID..].copy_from_slice(&key.vout.to_be_bytes());
    out
}

pub(crate) fn encode_receive(row: &ReceiveRow) -> ([u8; RECEIVE_KEY], [u8; RECEIVE_VALUE]) {
    (encode_receive_key(&row.key), row.value.into_u64().to_le_bytes())
}

/// Panics on an unknown kind tag or a value above the money supply (rows = sealed, checksummed
/// `encode_receive` output)
pub(crate) fn decode_receive(key: &[u8; RECEIVE_KEY], value: &[u8; RECEIVE_VALUE]) -> ReceiveRow {
    let field = |at: usize| -> [u8; 4] { key[at..at + 4].try_into().expect("4-byte field") };
    let at = AddressKey::LEN;
    let address = AddressKey {
        hash: key[..HASH160].try_into().expect("20-byte hash"),
        kind: AddressKind::decode(key[HASH160]).expect("receives: kind tag from encode_receive"),
    };
    let txid = <[u8; TXID]>::try_from(&key[at + 4..at + 4 + TXID]).expect("32-byte txid");
    ReceiveRow {
        key: ReceiveKey {
            address,
            height: BlockHeight::from_u32(u32::from_be_bytes(field(at))),
            txid: TxId::from_bytes(txid),
            vout: u32::from_be_bytes(field(at + 4 + TXID)),
        },
        value: Zatoshis::from_u64(u64::from_le_bytes(*value))
            .expect("receives: value within the money supply (from encode_receive)"),
    }
}

/// `txid ‖ vout` big-endian (spent output = spend row's key)
pub(crate) fn encode_spent_key(txid: &TxId, vout: u32) -> [u8; SPENT_KEY] {
    let mut out = [0u8; SPENT_KEY];
    out[..TXID].copy_from_slice(txid.as_ref());
    out[TXID..].copy_from_slice(&vout.to_be_bytes());
    out
}

/// `height ‖ spender` (key = `encode_spent_key`)
pub(crate) fn encode_spend(spend: &Spend) -> [u8; SPEND] {
    let mut out = [0u8; SPEND];
    out[..4].copy_from_slice(&u32::from(spend.height).to_be_bytes());
    out[4..].copy_from_slice(spend.spender.as_ref());
    out
}

pub(crate) fn decode_spend(bytes: &[u8; SPEND]) -> Spend {
    let (height, spender) = bytes.split_at(4);
    Spend {
        height: BlockHeight::from_u32(u32::from_be_bytes(
            height.try_into().expect("4-byte height"),
        )),
        spender: TxId::from_bytes(<[u8; TXID]>::try_from(spender).expect("32-byte txid")),
    }
}

#[cfg(test)]
mod tests {
    use zaino_source::testing::h;

    use super::*;

    /// Corrupt rows panic naming the invariant, never decode to a guess
    #[test]
    #[should_panic(expected = "receives: value within the money supply")]
    fn a_receive_value_above_the_money_supply_panics() {
        let receive = ReceiveKey {
            address: AddressKey::p2pkh([0x11; HASH160]),
            height: h(7),
            txid: TxId::from_bytes([0x22; TXID]),
            vout: 0,
        };
        decode_receive(&encode_receive_key(&receive), &u64::MAX.to_le_bytes());
    }

    #[test]
    #[should_panic(expected = "receives: kind tag from encode_receive")]
    fn a_receive_of_an_unknown_kind_panics() {
        let receive = ReceiveKey {
            address: AddressKey::p2pkh([0x11; HASH160]),
            height: h(7),
            txid: TxId::from_bytes([0x22; TXID]),
            vout: 0,
        };
        let mut key = encode_receive_key(&receive);
        key[HASH160] = 0x03;
        decode_receive(&key, &5u64.to_le_bytes());
    }

    /// `Ord` = the encoding byte for byte, every level of the composite key (range reads compare
    /// bytes: a disagreement silently reorders a scan)
    #[test]
    fn receive_keys_sort_like_their_encoded_bytes() {
        let addr = AddressKey::p2pkh([0x11; HASH160]);
        let txid = TxId::from_bytes([0x22; TXID]);
        let zat = |n| Zatoshis::from_u64(n).expect("in supply");
        let h = BlockHeight::from_u32;

        // ascending in each key part, one part changed at a time (address = hash, then kind)
        let ascending = [
            ReceiveKey { address: AddressKey::opaque(), height: h(9), txid, vout: 0 },
            ReceiveKey { address: AddressKey::p2pkh([0x11; HASH160]), height: h(7), txid, vout: 0 },
            ReceiveKey { address: addr, height: h(7), txid, vout: 1 },
            ReceiveKey {
                address: addr,
                height: h(7),
                txid: TxId::from_bytes([0x23; TXID]),
                vout: 0,
            },
            ReceiveKey {
                address: addr,
                height: h(8),
                txid: TxId::from_bytes([0x00; TXID]),
                vout: 0,
            },
            ReceiveKey { address: AddressKey::p2sh([0x11; HASH160]), height: h(0), txid, vout: 0 },
            ReceiveKey { address: AddressKey::p2pkh([0x12; HASH160]), height: h(0), txid, vout: 0 },
        ];
        for pair in ascending.windows(2) {
            let (low, high) = (pair[0], pair[1]);
            assert!(low < high, "{low:?} !< {high:?}");
            let (low_bytes, high_bytes) = (encode_receive_key(&low), encode_receive_key(&high));
            assert!(low_bytes < high_bytes, "encoding != Ord: {low:?} vs {high:?}");
            let row = ReceiveRow { key: low, value: zat(5) };
            assert_eq!(decode_receive(&low_bytes, &5u64.to_le_bytes()), row, "{low:?} round trip");
        }
    }

    /// `receives` row: hash ‖ kind ‖ height ‖ txid ‖ vout (big-endian) → zats (little-endian, a
    /// quantity)
    #[test]
    fn a_receive_row_is_its_golden_bytes_and_round_trips() {
        let receive = ReceiveRow {
            key: ReceiveKey {
                address: AddressKey::p2sh([0x09; HASH160]),
                height: h(0x0102_0304),
                txid: TxId::from_bytes([0x22; TXID]),
                vout: 0x0506_0708,
            },
            value: Zatoshis::from_u64(0x0a0b_0c0d).expect("in supply"),
        };
        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();

        let (key, value) = encode_receive(&receive);

        let row = [&key[..], &value[..]].concat();
        let golden: [(&str, String); 6] = [
            ("key: address hash", "09".repeat(HASH160)),
            ("key: kind p2sh", "01".into()),
            ("key: height BE", "01020304".into()),
            ("key: txid", "22".repeat(TXID)),
            ("key: vout BE", "05060708".into()),
            ("value: zats LE", "0d0c0b0a00000000".into()),
        ];
        let mut at = 0;
        for (field, expected) in golden {
            let width = expected.len() / 2;
            assert_eq!(row.get(at..at + width).map(hex), Some(expected), "{field} at byte {at}");
            at += width;
        }
        assert_eq!(hex(&row[at..]), "", "trailing bytes after byte {at}");
        assert_eq!(decode_receive(&key, &value), receive, "decode(encode(receive))");
    }

    /// `spent` row: txid ‖ vout (big-endian) → height ‖ spender
    #[test]
    fn a_spent_row_is_its_golden_bytes_and_round_trips() {
        let (txid, vout) = (TxId::from_bytes([0x22; TXID]), 0x0102_0304);
        let spend = Spend { height: h(0x0506_0708), spender: TxId::from_bytes([0x44; TXID]) };
        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();

        let row = [&encode_spent_key(&txid, vout)[..], &encode_spend(&spend)[..]].concat();

        let golden: [(&str, String); 4] = [
            ("key: spent txid", "22".repeat(TXID)),
            ("key: spent vout BE", "01020304".into()),
            ("value: height BE", "05060708".into()),
            ("value: spender txid", "44".repeat(TXID)),
        ];
        let mut at = 0;
        for (field, expected) in golden {
            let width = expected.len() / 2;
            assert_eq!(row.get(at..at + width).map(hex), Some(expected), "{field} at byte {at}");
            at += width;
        }
        assert_eq!(hex(&row[at..]), "", "trailing bytes after byte {at}");
        assert_eq!(decode_spend(&encode_spend(&spend)), spend, "decode(encode(spend))");
    }
}
