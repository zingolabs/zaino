//! Disk layouts of the two maps + the records they hold
//!
//! - big-endian throughout (keys compare as bytes: byte order = key order)
//! - derived `Ord` = that encoding (pinned by a test below)

use zaino_primitives::types::{TransactionId, Zatoshis};
use zcash_transparent::address::TransparentAddress;

const HASH160: usize = 20;
const TXID: usize = 32;

pub(crate) const RECEIVE_KEY: usize = AddressKey::LEN + 4 + TXID + 4;
pub(crate) const RECEIVE_VALUE: usize = 8;
pub(crate) const SPEND: usize = 4 + TXID;

/// Standard form an output's 20 bytes came from
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
enum AddressKind {
    P2pkh = 0x00,
    P2sh = 0x01,
    Opaque = 0x02,
}

impl AddressKind {
    fn decode(tag: u8) -> Option<Self> {
        match tag {
            0x00 => Some(Self::P2pkh),
            0x01 => Some(Self::P2sh),
            0x02 => Some(Self::Opaque),
            _ => None,
        }
    }
}

/// Fixed-width address tag `[hash160][kind]` (the whole identity a row is keyed by)
///
/// - 21 B = the addr id (interning into a `u64` breaks even at ~2.9 rows per address; TEX /
///   ephemeral receivers single-use by construction, `docs/design/index-data-structures.md` §7)
/// - hash first: its uniform bytes lead the key (an engine may shard on them)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AddressKey {
    hash: [u8; HASH160],
    kind: AddressKind,
}

impl AddressKey {
    pub(crate) const LEN: usize = 1 + HASH160;

    pub(crate) fn p2pkh(hash: [u8; HASH160]) -> Self {
        Self { kind: AddressKind::P2pkh, hash }
    }

    pub(crate) fn p2sh(hash: [u8; HASH160]) -> Self {
        Self { kind: AddressKind::P2sh, hash }
    }

    /// Every non-standard output, under one key (no address string parses to it)
    pub(crate) fn opaque() -> Self {
        Self { kind: AddressKind::Opaque, hash: [0u8; HASH160] }
    }
}

impl From<&TransparentAddress> for AddressKey {
    fn from(address: &TransparentAddress) -> Self {
        match *address {
            TransparentAddress::PublicKeyHash(hash) => Self::p2pkh(hash),
            TransparentAddress::ScriptHash(hash) => Self::p2sh(hash),
        }
    }
}

/// `addr ‖ height ‖ txid ‖ vout` (range-scanned per address: height directly after it)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ReceiveKey {
    pub(crate) address: AddressKey,
    pub(crate) height: u32,
    pub(crate) txid: TransactionId,
    pub(crate) vout: u32,
}

impl ReceiveKey {
    /// Lowest key `address` can hold at `height` (a scan bound, never a stored row)
    pub(crate) fn first(address: AddressKey, height: u32) -> Self {
        Self { address, height, txid: TransactionId::from([0u8; TXID]), vout: 0 }
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
    pub(crate) height: u32,
    pub(crate) spender: TransactionId,
}

pub(crate) fn encode_receive_key(key: &ReceiveKey) -> [u8; RECEIVE_KEY] {
    let mut out = [0u8; RECEIVE_KEY];
    out[..HASH160].copy_from_slice(&key.address.hash);
    out[HASH160] = key.address.kind as u8;
    let at = AddressKey::LEN;
    out[at..at + 4].copy_from_slice(&key.height.to_be_bytes());
    out[at + 4..at + 4 + TXID].copy_from_slice(&<[u8; TXID]>::from(key.txid));
    out[at + 4 + TXID..].copy_from_slice(&key.vout.to_be_bytes());
    out
}

pub(crate) fn encode_receive(row: &ReceiveRow) -> ([u8; RECEIVE_KEY], [u8; RECEIVE_VALUE]) {
    (encode_receive_key(&row.key), row.value.as_u64().to_be_bytes())
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
            height: u32::from_be_bytes(field(at)),
            txid: TransactionId::from(txid),
            vout: u32::from_be_bytes(field(at + 4 + TXID)),
        },
        value: Zatoshis::new(u64::from_be_bytes(*value))
            .expect("receives: value within the money supply (from encode_receive)"),
    }
}

/// `height ‖ spender` (key = `OutPoint::encode`)
pub(crate) fn encode_spend(spend: &Spend) -> [u8; SPEND] {
    let mut out = [0u8; SPEND];
    out[..4].copy_from_slice(&spend.height.to_be_bytes());
    out[4..].copy_from_slice(&<[u8; TXID]>::from(spend.spender));
    out
}

pub(crate) fn decode_spend(bytes: &[u8; SPEND]) -> Spend {
    let (height, spender) = bytes.split_at(4);
    Spend {
        height: u32::from_be_bytes(height.try_into().expect("4-byte height")),
        spender: TransactionId::from(<[u8; TXID]>::try_from(spender).expect("32-byte txid")),
    }
}

#[cfg(test)]
mod tests {
    use std::panic::catch_unwind;

    use super::*;

    /// `Ord` = the encoding byte for byte, every level of the composite key (range reads compare
    /// bytes: a disagreement silently reorders a scan); golden bytes of both maps' rows
    #[test]
    fn encoded_order_is_key_order_and_every_field_round_trips() {
        let addr = AddressKey::p2pkh([0x11; HASH160]);
        let txid = TransactionId::from([0x22; TXID]);
        let zat = |n| Zatoshis::new(n).expect("in supply");

        // ascending in each key part, one part changed at a time (address = hash, then kind)
        let ascending = [
            ReceiveKey { address: AddressKey::opaque(), height: 9, txid, vout: 0 },
            ReceiveKey { address: AddressKey::p2pkh([0x11; HASH160]), height: 7, txid, vout: 0 },
            ReceiveKey { address: addr, height: 7, txid, vout: 1 },
            ReceiveKey {
                address: addr,
                height: 7,
                txid: TransactionId::from([0x23; TXID]),
                vout: 0,
            },
            ReceiveKey {
                address: addr,
                height: 8,
                txid: TransactionId::from([0x00; TXID]),
                vout: 0,
            },
            ReceiveKey { address: AddressKey::p2sh([0x11; HASH160]), height: 0, txid, vout: 0 },
            ReceiveKey { address: AddressKey::p2pkh([0x12; HASH160]), height: 0, txid, vout: 0 },
        ];
        for pair in ascending.windows(2) {
            let (low, high) = (pair[0], pair[1]);
            assert!(low < high, "{low:?} !< {high:?}");
            let (low_bytes, high_bytes) = (encode_receive_key(&low), encode_receive_key(&high));
            assert!(low_bytes < high_bytes, "encoding != Ord: {low:?} vs {high:?}");
            let row = ReceiveRow { key: low, value: zat(5) };
            assert_eq!(decode_receive(&low_bytes, &5u64.to_be_bytes()), row);
        }

        let receive = ReceiveRow {
            key: ReceiveKey {
                address: AddressKey::p2sh([0x09; HASH160]),
                height: 900,
                txid,
                vout: 3,
            },
            value: zat(1_234_567),
        };
        let golden_key =
            [&[0x09; HASH160][..], &[0x01], &[0, 0, 0x03, 0x84], &[0x22; TXID], &[0, 0, 0, 3]]
                .concat();
        let (key, value) = encode_receive(&receive);
        assert_eq!(key.as_slice(), golden_key, "hash ‖ kind ‖ height ‖ txid ‖ vout, big-endian");
        assert_eq!(value, [0, 0, 0, 0, 0, 0x12, 0xd6, 0x87], "zats big-endian");
        assert_eq!(decode_receive(&key, &value), receive);

        // corrupt rows panic naming the invariant, never decode to a guess
        let above_supply = catch_unwind(|| decode_receive(&key, &u64::MAX.to_be_bytes()));
        assert!(above_supply.is_err(), "value above the money supply decoded");
        let mut unknown_kind = key;
        unknown_kind[HASH160] = 0x03;
        let unknown = catch_unwind(|| decode_receive(&unknown_kind, &value));
        assert!(unknown.is_err(), "unknown kind tag decoded");

        let spend = Spend { height: 900, spender: TransactionId::from([0x44; TXID]) };
        let golden = [&[0, 0, 0x03, 0x84][..], &[0x44; TXID]].concat();
        assert_eq!(encode_spend(&spend).as_slice(), golden, "height ‖ spender, big-endian");
        assert_eq!(decode_spend(&encode_spend(&spend)), spend);
    }
}
