//! What a segment stores: fixed-width records, ordered by a big-endian key

use zaino_primitives::types::{OutPoint, TransactionId};

/// Ordering key, encoded **big-endian** (byte order = key order: segments compare encoded bytes)
///
/// - composite key = parts most-significant first (one little-endian atom silently reorders)
pub trait Key: Ord + Sized + Send {
    /// Bytes at the front of a record
    const LEN: usize;

    /// Exact-key lookups ([`Snapshot::get`](super::Snapshot::get)) → a filter per segment
    ///
    /// - first 8 encoded bytes must be uniform (a hash, a txid): the filter shards on them
    const PROBED: bool = false;

    /// Exactly [`LEN`](Self::LEN) bytes
    fn encode(&self) -> Vec<u8>;

    /// `None` = shorter than [`LEN`](Self::LEN) or undecodable
    fn decode(bytes: &[u8]) -> Option<Self>;
}

/// Fixed width, key first (record `n` at `n * STRIDE`: no offset table, no merge framing)
pub trait Record: Sized + Send {
    type Key: Key;

    /// Bytes per record, key included
    const STRIDE: usize;

    fn key(&self) -> Self::Key;

    /// Exactly [`STRIDE`](Self::STRIDE) bytes, key first
    fn encode(&self, out: &mut Vec<u8>);

    /// `None` = shorter than [`STRIDE`](Self::STRIDE) or undecodable
    fn decode(bytes: &[u8]) -> Option<Self>;
}

/// `txid ‖ vout` (probed, never scanned: a txid = the uniform prefix a filter shards on)
impl Key for OutPoint {
    const LEN: usize = TXID + 4;
    const PROBED: bool = true;

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&<[u8; TXID]>::from(self.txid));
        out.extend_from_slice(&self.vout.to_be_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            txid: TransactionId::from(<[u8; TXID]>::try_from(bytes.get(..TXID)?).ok()?),
            vout: u32::from_be_bytes(bytes.get(TXID..Self::LEN)?.try_into().ok()?),
        })
    }
}

const TXID: usize = 32;

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden layout: txid bytes as held, then `vout` big-endian (so byte order = key order)
    #[test]
    fn outpoint_key_layout_is_pinned_and_orders_like_the_key() {
        let outpoint = OutPoint { txid: TransactionId::from([0xab; TXID]), vout: 0x0102_0304 };
        let bytes = outpoint.encode();
        let golden = [&[0xab; TXID][..], &[0x01, 0x02, 0x03, 0x04]].concat();
        assert_eq!(bytes, golden);
        assert_eq!(OutPoint::decode(&bytes), Some(outpoint), "round trip");
        assert_eq!(OutPoint::decode(&bytes[..TXID + 3]), None, "short");

        let later = OutPoint { vout: 0x0102_0305, ..outpoint };
        assert!(outpoint < later && bytes < later.encode(), "derived Ord = byte order");
    }
}
