//! Key layouts for the two segment sets + the records they order
//!
//! - big-endian throughout (`zaino_persistence::lsm` compares encoded prefixes: byte order = key
//!   order)
//! - derived `Ord` = that encoding (pinned by a test below)

use zaino_persistence::lsm::{Key, Record};
use zaino_primitives::types::{OutPoint, TransactionId, Zatoshis};
use zcash_transparent::address::TransparentAddress;

const HASH160: usize = 20;
const TXID: usize = 32;

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
/// - hash first: its uniform bytes lead the key, so the receives filter can shard on them
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

    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.hash);
        out.push(self.kind as u8);
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            hash: bytes.get(..HASH160)?.try_into().ok()?,
            kind: AddressKind::decode(*bytes.get(HASH160)?)?,
        })
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

impl Key for ReceiveKey {
    const LEN: usize = AddressKey::LEN + 4 + TXID + 4;
    /// A lookup is one address's history: segments without the address are skipped
    const FILTER_PREFIX: usize = AddressKey::LEN;

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        self.address.encode_into(&mut out);
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(&<[u8; TXID]>::from(self.txid));
        out.extend_from_slice(&self.vout.to_be_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let at = AddressKey::LEN;
        Some(Self {
            address: AddressKey::decode(bytes)?,
            height: u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?),
            txid: TransactionId::from(<[u8; TXID]>::try_from(bytes.get(at + 4..at + 36)?).ok()?),
            vout: u32::from_be_bytes(bytes.get(at + 36..at + 40)?.try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReceiveRow {
    pub(crate) key: ReceiveKey,
    pub(crate) value: Zatoshis,
}

impl Record for ReceiveRow {
    type Key = ReceiveKey;

    const STRIDE: usize = <ReceiveKey as Key>::LEN + 8;

    fn key(&self) -> ReceiveKey {
        self.key
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.key.encode());
        out.extend_from_slice(&self.value.as_u64().to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let at = <ReceiveKey as Key>::LEN;
        Some(Self {
            key: ReceiveKey::decode(bytes)?,
            value: Zatoshis::new(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
                .ok()?,
        })
    }
}

/// What spent an outpoint (the non-finalized map's value, a `spent` row's payload)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spend {
    pub(crate) height: u32,
    pub(crate) spender: TransactionId,
}

/// `outpoint → spend`, probed by outpoint
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpentRow {
    pub(crate) key: OutPoint,
    pub(crate) spend: Spend,
}

impl Record for SpentRow {
    type Key = OutPoint;

    const STRIDE: usize = <OutPoint as Key>::LEN + 4 + TXID;

    fn key(&self) -> OutPoint {
        self.key
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.key.encode());
        out.extend_from_slice(&self.spend.height.to_be_bytes());
        out.extend_from_slice(&<[u8; TXID]>::from(self.spend.spender));
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let at = <OutPoint as Key>::LEN;
        let height = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?);
        let spender =
            TransactionId::from(<[u8; TXID]>::try_from(bytes.get(at + 4..at + 36)?).ok()?);
        Some(Self { key: OutPoint::decode(bytes)?, spend: Spend { height, spender } })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded<R: Record>(record: &R) -> Vec<u8> {
        let mut out = Vec::new();
        record.encode(&mut out);
        out
    }

    /// `Ord` = the encoding byte for byte, every level of both composite keys (segment search
    /// compares encoded prefixes: a disagreement silently reorders a segment)
    #[test]
    fn encoded_order_is_key_order_and_every_field_round_trips() {
        let addr = AddressKey::p2pkh([0x11; HASH160]);
        let txid = TransactionId::from([0x22; TXID]);

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
            assert!(low.encode() < high.encode(), "encoding != Ord: {low:?} vs {high:?}");
            assert_eq!(ReceiveKey::decode(&low.encode()), Some(low));
        }

        // fixed widths (segment stride = arithmetic: a wrong LEN corrupts every later row)
        assert_eq!(<ReceiveKey as Key>::LEN, 61);
        assert_eq!(ReceiveRow::STRIDE, 69);
        assert_eq!(<OutPoint as Key>::LEN, 36);
        assert_eq!(SpentRow::STRIDE, 72);

        let receive =
            ReceiveRow { key: ascending[0], value: Zatoshis::new(1_234_567).expect("in supply") };
        let bytes = encoded(&receive);
        assert_eq!(bytes.len(), ReceiveRow::STRIDE);
        assert_eq!(&bytes[..ReceiveKey::LEN], &ascending[0].encode()[..]);
        assert_eq!(ReceiveRow::decode(&bytes), Some(receive));

        // above the money supply = not a value (decode refuses)
        let mut corrupt = bytes.clone();
        corrupt[ReceiveKey::LEN..].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(ReceiveRow::decode(&corrupt), None);

        let spent = SpentRow {
            key: OutPoint { txid, vout: 3 },
            spend: Spend { height: 900, spender: TransactionId::from([0x44; TXID]) },
        };
        let bytes = encoded(&spent);
        let golden =
            [&[0x22; TXID][..], &[0, 0, 0, 3], &[0, 0, 0x03, 0x84], &[0x44; TXID]].concat();
        assert_eq!(bytes, golden, "outpoint ‖ height ‖ spender, big-endian");
        assert_eq!(SpentRow::decode(&bytes), Some(spent));

        // tag bytes = the on-disk contract (unknown one refused, never guessed)
        assert_eq!(AddressKey::decode(&[0x03; AddressKey::LEN]), None);
        let p2sh = AddressKey::p2sh([9; HASH160]);
        assert_eq!(AddressKey::decode(&p2sh.encode_bytes()), Some(p2sh));
    }

    impl AddressKey {
        fn encode_bytes(&self) -> Vec<u8> {
            let mut out = Vec::new();
            self.encode_into(&mut out);
            out
        }
    }
}
